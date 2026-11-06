// SPDX-License-Identifier: MIT OR Apache-2.0

//! Listing a whole subtree, folder by folder.

use std::collections::VecDeque;

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;

use crate::{Entry, ProviderError, RemotePath, StorageProvider};

/// Every file and folder below `root` (not `root` itself), each once, in no
/// particular order. Folders are listed side by side, at most
/// `capabilities().list_concurrency` at a time. The first listing that
/// fails ends the walk with its error.
///
/// Only entries strictly inside the folder listed are taken. The folder
/// itself (S3 lists the marker `a/` under the prefix `a/`) would be walked
/// again forever, and a path from elsewhere would lead out of `root`.
pub async fn walk(
    provider: &dyn StorageProvider,
    root: &RemotePath,
) -> Result<Vec<Entry>, ProviderError> {
    let limit = provider.capabilities().list_concurrency.max(1);
    let mut waiting = VecDeque::from([root.clone()]);
    let mut listing = FuturesUnordered::new();
    let mut found = Vec::new();
    loop {
        while listing.len() < limit
            && let Some(dir) = waiting.pop_front()
        {
            listing.push(async move {
                let result = provider.list(&dir).await;
                (dir, result)
            });
        }
        let Some((dir, result)) = listing.next().await else {
            return Ok(found);
        };
        for entry in result?.entries {
            if entry.path == dir || !entry.path.is_within(&dir) {
                continue;
            }
            if entry.is_folder {
                waiting.push_back(entry.path.clone());
            }
            found.push(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AuthHint, ByteStream, Capabilities, Listing, MockOp, MockProvider, UploadOptions,
        UploadSource,
    };
    use std::time::Duration;

    fn p(path: &str) -> RemotePath {
        RemotePath::parse(path).unwrap()
    }

    /// Three levels of folders (`a`, `a/b`, `a/b/c`) and seven files, one of them a lookalike
    /// sibling of `a`.
    fn tree() -> MockProvider {
        MockProvider::new(Capabilities::default())
            .with_file("index.html", b"1")
            .with_file("a-lookalike.txt", b"2")
            .with_file("a/one.txt", b"3")
            .with_file("a/two.txt", b"4")
            .with_file("a/b/three.txt", b"5")
            .with_file("a/b/c/four.txt", b"6")
            .with_file("a/b/c/five.txt", b"7")
    }

    /// Path and folder flag of each entry, sorted, so the walk's order does not matter.
    fn found(entries: &[Entry]) -> Vec<(&str, bool)> {
        let mut found: Vec<_> = entries
            .iter()
            .map(|e| (e.path.as_str(), e.is_folder))
            .collect();
        found.sort();
        found
    }

    #[tokio::test]
    async fn walk_returns_every_entry_below_root_once() {
        let m = tree();
        let entries = walk(&m, &RemotePath::root()).await.unwrap();
        assert_eq!(
            found(&entries),
            [
                ("a", true),
                ("a-lookalike.txt", false),
                ("a/b", true),
                ("a/b/c", true),
                ("a/b/c/five.txt", false),
                ("a/b/c/four.txt", false),
                ("a/b/three.txt", false),
                ("a/one.txt", false),
                ("a/two.txt", false),
                ("index.html", false),
            ]
        );
        // The root and each of the three folders are listed once.
        assert_eq!(m.calls(MockOp::List), 4);
    }

    #[tokio::test]
    async fn walk_below_a_folder_leaves_out_the_folder_and_its_lookalikes() {
        let m = tree();
        let entries = walk(&m, &p("a/b")).await.unwrap();
        assert_eq!(
            found(&entries),
            [
                ("a/b/c", true),
                ("a/b/c/five.txt", false),
                ("a/b/c/four.txt", false),
                ("a/b/three.txt", false),
            ]
        );
        let leaf = walk(&m, &p("a/b/c")).await.unwrap();
        assert_eq!(leaf.len(), 2);
        assert!(leaf.iter().all(|e| e.size == 1 && !e.is_folder));
    }

    #[tokio::test]
    async fn walk_of_an_empty_folder_finds_nothing() {
        let m = MockProvider::new(Capabilities::default()).with_folder("empty");
        assert!(walk(&m, &p("empty")).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn walk_fails_with_the_first_listing_error() {
        let m = tree();
        assert_eq!(
            walk(&m, &p("missing")).await.unwrap_err(),
            ProviderError::NotFound
        );
        let auth = ProviderError::Auth {
            hint: AuthHint::Unknown,
        };
        m.fail_next(MockOp::List, auth.clone(), 1);
        assert_eq!(walk(&m, &RemotePath::root()).await.unwrap_err(), auth);
    }

    #[tokio::test(start_paused = true)]
    async fn walk_respects_list_concurrency() {
        let caps = Capabilities {
            list_concurrency: 2,
            ..Capabilities::default()
        };
        let mut m = MockProvider::new(caps);
        for folder in 0..6 {
            for inner in 0..3 {
                m = m.with_file(&format!("d{folder}/e{inner}/f.txt"), b"x");
            }
        }
        m.set_latency(MockOp::List, Duration::from_millis(50));
        let entries = walk(&m, &RemotePath::root()).await.unwrap();
        assert_eq!(entries.len(), 6 + 18 + 18);
        assert_eq!(m.calls(MockOp::List), 1 + 6 + 18);
        // Never more than the provider allows, and the walk does use what it allows.
        assert_eq!(m.max_in_flight(MockOp::List), 2);
    }

    /// Lists like the mock inside, plus the folder listed and a folder from elsewhere: what a
    /// broken listing returns, or S3 when the marker `a/` comes back under the prefix `a/`.
    struct Stray(MockProvider);

    fn folder(path: RemotePath) -> Entry {
        Entry {
            path,
            is_folder: true,
            size: 0,
            modified: None,
            checksum: None,
            content_type: None,
            replicated_regions: Vec::new(),
        }
    }

    #[async_trait::async_trait]
    impl StorageProvider for Stray {
        fn capabilities(&self) -> &Capabilities {
            self.0.capabilities()
        }
        fn endpoint(&self) -> String {
            self.0.endpoint()
        }
        async fn list(&self, dir: &RemotePath) -> Result<Listing, ProviderError> {
            let mut listing = self.0.list(dir).await?;
            listing.entries.push(folder(dir.clone()));
            listing.entries.push(folder(p("elsewhere")));
            Ok(listing)
        }
        async fn upload(
            &self,
            path: &RemotePath,
            source: UploadSource,
            opts: &UploadOptions,
        ) -> Result<(), ProviderError> {
            self.0.upload(path, source, opts).await
        }
        async fn download(&self, path: &RemotePath) -> Result<ByteStream, ProviderError> {
            self.0.download(path).await
        }
        async fn delete_file(&self, path: &RemotePath) -> Result<(), ProviderError> {
            self.0.delete_file(path).await
        }
        async fn delete_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
            self.0.delete_folder(dir).await
        }
        async fn create_folder(&self, dir: &RemotePath) -> Result<(), ProviderError> {
            self.0.create_folder(dir).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn walk_skips_self_and_stray_entries() {
        let m = Stray(
            MockProvider::new(Capabilities::default())
                .with_file("a/one.txt", b"1")
                .with_file("a/b/two.txt", b"2")
                .with_file("elsewhere/x.txt", b"3"),
        );
        // A walk that went in circles would never give the clock back without this.
        m.0.set_latency(MockOp::List, Duration::from_millis(1));
        let walked = tokio::time::timeout(Duration::from_secs(5), walk(&m, &p("a")))
            .await
            .expect("the walk ends")
            .unwrap();
        assert_eq!(
            found(&walked),
            [("a/b", true), ("a/b/two.txt", false), ("a/one.txt", false)]
        );
        assert_eq!(m.0.calls(MockOp::List), 2, "a and a/b, once each");
    }
}
