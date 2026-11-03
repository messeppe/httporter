# Bunny Storage: behaviour seen live (Task 7)

Probed on 2026-11-03 against the test zone `httporter-test-sg` (region `sg`, `sg.storage.bunnycdn.com`) with `crates/httporter-bunny/tests/live.rs`:

```text
cargo test --locked -p httporter-bunny --test live -- --ignored --nocapture
```

Every probe works in its own `probe-<time>-<name>/` folder and deletes it at the end; the zone root had 0 entries before and after every run. The account API (`api.bunny.net`, where `page=` paging lives) was not called, and no pull zone was purged. Quoted bodies are Bunny's own words, exactly as sent.

## Folders

- **B1, `PUT /{zone}/{dir}/` with an empty body makes an empty folder.** Answer `201 {"HttpCode":201,"Message":"Directory created."}`. The parent lists it as `IsDirectory: true, Length: 0, Checksum: null`, and it lists as `200 []`. Missing parents are made too (`PUT n1/n2/` made both).
- **The same `PUT` on a folder that is already there is refused:** `400 {"HttpCode":400,"Message":"Unable to upload file. Invalid path specified."}`, whether the folder is empty or holds files. The files are kept.
- **A folder `PUT` next to a file of the same name succeeds** (`PUT keep.txt/` with a file `keep.txt`): the parent then lists both `keep.txt` and `keep.txt/`, and the file still downloads.
- **Empty folders stay.** After deleting the only file in `gone/`, the parent still lists `gone/`, and `gone/` lists as `[]`.
- Consequences, applied in the client: `EmptyFolders::Supported`; `create_folder` is that `PUT`, and a 400 is settled by the parent's listing (the folder is there: done; it is not: the refusal, quoting Bunny). Keeping a file and a folder of one name apart is left to the planner, as for S3 (ruling Q34).

## Listings

- **A listing is one JSON array with every child; there are no pages.** A folder of 1,050 files came back whole in one `200` answer of about 532 KB. The client lists all 1,050.
- **Shape of one object** (types only; some values, like `UserId`, name the account): `ArrayNumber: number, Checksum: string|null, ContentType: string, DateCreated: string, Guid: string, IsDirectory: bool, LastChanged: string, Length: number, ObjectName: string, Path: string, ReplicatedZones: string|null, ServerId: number, StorageZoneId: number, StorageZoneName: string, UserId: string`. Headers: `content-type: application/json`, chunked (`transfer-encoding`), no paging headers.
- **A listing never answers 404.** `200 []` comes back for an empty folder, a folder that is not there (whether or not its parent exists), and a file listed as a folder (`f.txt/`).
- Consequence, applied in the client: an empty answer for anything but the root is checked against the parent's listing, and is `NotFound` unless the parent shows a folder of exactly that name. That costs one more listing per empty folder.

## Names

- **Case-sensitive.** With only `Case.txt` stored, `GET case.txt` is `404`; uploading `case.txt` then keeps both, each with its own content. `dir/` lists as `[]` (now `NotFound` in the client) when only `Dir/` exists.
- **Encoded names round-trip.** Uploaded through the client (every segment percent-encoded), these list under the same name and download the same bytes: `a (1)+b.txt`, `plus+sign.txt`, `space name.txt`, `üñ 日本.txt`, `hash#percent%.txt`, `semi;comma,eq=amp&.txt`, `quote'tilde~[x].txt`.
- **Unencoded `+` and parentheses are taken literally** (`PUT raw+plus.txt` lists as `raw+plus.txt`, not `raw plus.txt`; `raw(paren).txt` as is). The client encodes them anyway.

## Uploads and checksums

- **A wrong `Checksum`:** `400 {"HttpCode":400,"Message":"Unable to upload file: Exception when writing file: Checksum missmatch, skipping commit"}` (sic), and the file is not stored (`GET` is 404). The client maps it to `ChecksumMismatch`; the words begin "Unable to" but are not the busy answer, which the client tells apart by "Unable to process request".
- **A lowercase `Checksum` is accepted** (201). The client sends uppercase, as documented.
- **The listing's `Checksum` is the SHA-256 in uppercase hex.**
- Success is `201 {"HttpCode":201,"Message":"File uploaded."}`.

## Busy answers

- **Not triggered.** 60 listings at once (twelve times the client's limit of five) all got `200`, and 60 uploads at once all got `201`, from one PC to `sg`. The client keeps matching "Unable to process request" (spec P16) as the busy 400; the real wording is still unconfirmed.

## Time

- **`LastChanged` and `DateCreated` are UTC without a zone designator**, for example `"2026-11-03T07:49:00.859"` (milliseconds). The client reads them as UTC.
- On the test PC (UTC+07:00) they were about 100 s before the upload as the PC's clock saw it, and the answer's `Date` header was 102 s behind the PC's clock too: the PC's clock runs ahead, and Bunny's times agree with its own `Date`. Comparing a remote `LastChanged` with a local file time therefore carries the PC's clock error; a tolerance of a few minutes, or a size/checksum comparison, is safer than exact times.

## Deletes

- **A missing file or folder:** `DELETE` answers `404 {"HttpCode":404,"Message":"Object Not Found"}`; the client says `NotFound`. A `GET` of a missing file is the same 404.
- **A folder `DELETE` is recursive and answers only when the whole subtree is gone:** `200 {"HttpCode":200,"Message":"Directory deleted successfully."}`. It is slow: 1,050 small files took **128 s** (about 8 files/s).
- **Bunny goes on deleting after the client gives up.** With a 10 s client timeout, the folder held 924, then 840, then 594 files at 0, 10 and 20 s afterwards; a second `DELETE` finished the rest in 65 s.
- Consequence, **not applied** (outside this crate): with the Transfers request timeout (60, 120 or 300 s), deleting a large folder (the reference export has 13,793 files: roughly half an hour at this rate) times out as a network error and is retried; each retry continues the server's work, but the retries can run out first. The engine or the app needs a strategy: a much longer timeout for folder deletes, treating a timed-out folder delete as in progress and checking by listing, or deleting the files one by one (16 at once) before the folders.
- Once, in the first full run, a probe folder was still there after the cleanup's `delete_folder` returned. That was right after the 1,050-file delete had timed out on the client and was still running on the server. Not seen again in three later runs; the probe's cleanup now checks and retries.

## Not probed

- The account API (zone discovery, `page=0`): these tests never call account-level APIs.
- Pull-zone purges: never called.
- Regions other than `sg`.
