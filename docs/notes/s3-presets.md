# S3 presets: addressing and quirks (spec B8)

Written for Task 8 (`crates/httporter-s3`), read on 2026-11-02. Each fact carries its basis:

- **[docs]** read on the service's own documentation page in this session;
- **[known]** general knowledge of the service, not re-read, so treat it as a lead to confirm;
- **[live]** to confirm when a real bucket is available (Task 11 uses R2; the rest have no test account).

## At a glance

| Preset | Base address | Signing region | Addressing | Region list |
| --- | --- | --- | --- | --- |
| Amazon S3 | `https://s3.{region}.amazonaws.com` (`.amazonaws.com.cn` for `cn-*`) | the chosen region | virtual-hosted | `REGIONS_AWS` (36) |
| Cloudflare R2 | `https://{account}.r2.cloudflarestorage.com` | `auto` | path | none |
| Backblaze B2 | `https://s3.{region}.backblazeb2.com` | the chosen region | path | `REGIONS_B2` (6) |
| Wasabi | `https://s3.{region}.wasabisys.com` | the chosen region | path | `REGIONS_WASABI` (16) |
| DigitalOcean Spaces | `https://{region}.digitaloceanspaces.com` | the chosen region | path | `REGIONS_SPACES` (13) |
| MinIO | the typed address (`http://localhost:9000`) | `us-east-1` unless typed | path | none |
| Other S3 | the typed address | `us-east-1` unless typed | the user's toggle | none |

Fixed in `endpoint()`: only "Other S3" reads the path-style toggle. `bucket_url` and `object_url` still use path style, whatever the endpoint says, when the bucket cannot be a host name (see Amazon S3 and "IP addresses" below).

## Common to every service

- SigV4, service name `s3`, header `x-amz-content-sha256` on every request. Uploads over HTTPS may send `UNSIGNED-PAYLOAD` [docs, AWS].
- Canonical path and query use one round of percent-encoding: everything except `A-Z a-z 0-9 - _ . ~`, hex in upper case, space as `%20` never `+` [docs, AWS]. Paths are not normalised (`//` stays) [known].
- ETag is the hex MD5 of the body only for a single-part upload without SSE-KMS/SSE-C. A multipart ETag is `md5(concat(part md5s))-N` [docs, AWS and R2]. Treat an ETag as an MD5 only when it is 32 hex characters.
- Integrity: send `Content-MD5`, store `x-amz-meta-httporter-sha256`. Do not send `x-amz-checksum-*` headers or `aws-chunked` trailers: support differs per service (R2 lists only some algorithms and is silent on PutObject/UploadPart [docs]) and one third-party project reports B2 rejecting `x-amz-checksum-algorithm` [live].
- `DeleteObjects`: at most 1,000 keys per request [docs, AWS]. AWS requires `Content-MD5` on the body for general purpose buckets [docs, AWS]; send it everywhere.
- Multipart: parts of at least 5 MiB except the last, at most 10,000 parts [docs, AWS, R2, DigitalOcean].

## Amazon S3

- Endpoints `s3.{region}.amazonaws.com` for all 34 commercial regions and the two GovCloud regions; both virtual-hosted and path-style URLs work in every region [docs]. The region list in `REGIONS_AWS` is that page's list.
- Virtual-hosted is the default. The TLS wildcard certificate matches only buckets without dots, so a dotted bucket (`static.example.com`) is addressed by path [docs]. Upper case, `_` and other non-host characters fall back the same way.
- China regions (`cn-north-1`, `cn-northwest-1`) live on `s3.{region}.amazonaws.com.cn` and use separate accounts [known]. They are handled in `endpoint()` but not listed in `REGIONS_AWS`.
- The signing region must be the bucket's region. A wrong one comes back as `AuthorizationHeaderMalformed` or a `PermanentRedirect` naming the right region [known]; Task 9 maps the former to the "Region" field.
- Directory buckets (S3 Express) need `Content-MD5` or an `x-amz-checksum-*` header on `DeleteObjects` and use zonal endpoints; they are out of scope [docs].

## Cloudflare R2

- `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`; region `auto`, and an empty region or `us-east-1` are aliases [docs].
- Jurisdictions use `<ACCOUNT_ID>.eu.r2.cloudflarestorage.com` (EU) and `<ACCOUNT_ID>.fedramp.r2.cloudflarestorage.com` (FedRAMP, inferred from the pattern); a jurisdiction bucket cannot be reached through any other endpoint [docs]. The R2 preset only builds the default address. Jurisdiction users choose "Other S3", type that address, region `auto`, path-style on.
- Both addressing styles appear in the docs; the preset uses path style [docs].
- Multipart: every part except the last must be the same size, at least 5 MiB, at most 5 GiB; incomplete uploads are aborted after 7 days [docs]. Task 10's 16 MiB parts comply.
- Re-uploading a part number replaces the earlier part; if that retry fails, the original part is lost [docs]. After a failed `UploadPart`, treat the part as not done and resend it; never rely on the earlier copy.
- Supported: `ListObjectsV2` (`prefix`, `delimiter`, `start-after`, `continuation-token`, `max-keys`, `encoding-type`, `fetch-owner`), `HeadObject`, `PutObject` with `Content-MD5`, `Content-Type`, `Cache-Control`, `DeleteObject(s)`, `CopyObject` (with `x-amz-metadata-directive`), `UploadPartCopy`, `ListBuckets`, `CreateBucket` [docs].
- Not supported: `x-amz-tagging`, SSE-KMS, object lock, ACL headers, website redirect; storage class only `STANDARD` and `STANDARD_IA`; `CopyObject` ignores checksum algorithm, tagging and ACL headers [docs]. Never send them.
- Checksums: full-object type only CRC-64/NVME; composite CRC-32, CRC-32C, SHA-1, SHA-256 [docs].
- Presigned URLs: the host is signed; expiry 1 s to 7 days [docs] (I4).
- ETag, `DeleteObjects` key limit and listing page size beyond the above are not stated on the pages read [live, Task 11].

## Backblaze B2

- `https://s3.<region>.backblazeb2.com`, for example `s3.us-east-005.backblazeb2.com`. HTTPS only ("Non-secure connections are rejected"); only SigV4 [docs].
- Both addressing styles are accepted in direct HTTP calls [docs]; the preset uses path style.
- An account lives in one data region (US East, US West, EU Central, CA East) fixed at sign-up [docs]. The codes in `REGIONS_B2` (`us-west-001`, `us-west-002`, `us-west-004`, `us-east-005`, `eu-central-003`, `ca-east-006`) come from memory; the console's "S3 Endpoint" field is authoritative [known]. The Add Site sheet should accept a typed region.
- Listed as supported: `PutObject`, `CopyObject`, `UploadPart`, `UploadPartCopy`, `DeleteObjects`, `ListObjectsV2`, the multipart set, object lock, versioning [docs].
- Not supported: ACLs (a bucket ACL call succeeds only when it matches the current one), IAM roles, object tagging, website configuration, browser POST uploads [docs].
- `x-amz-tagging` is rejected on `PutObject` and `CopyObject` [docs]. Never send it.
- Checksum data is not kept for copied or replicated objects, including copied parts [docs]. After a server-side move, compare by size and time or re-hash, not by the source's stored checksum.
- Files uploaded or hidden in the same second may be processed out of order; wait one second between them [docs].
- ETag meaning (B2 stores SHA-1 natively), `DeleteObjects` key limit and multipart part limits are not stated on the pages read [live]. A 40-hex ETag would be a SHA-1, not an MD5.

## Wasabi

- `https://s3.<region>.wasabisys.com`; `us-east-1` also answers on `s3.wasabisys.com`, and several EU regions have aliases (`s3.nl-1`, `s3.de-1`, `s3.uk-1`, …) that the preset does not use [docs].
- Use the endpoint of the bucket's region: a wrong endpoint still allows `GET` but refuses `PUT` and `DELETE` [docs].
- Both addressing styles work; Wasabi recommends path style for the freedom it gives bucket names [docs].
- SigV2 and SigV4 are supported; V4 works in every region [docs]. Browser POST uploads and SOAP are not supported [docs].
- Operation-level limits (`DeleteObjects`, multipart sizes) are in the Wasabi S3 API reference, not read here [live].

## DigitalOcean Spaces

- `https://<region>.digitaloceanspaces.com`; the docs show the virtual-hosted form `<space>.<region>.digitaloceanspaces.com` [docs]. Path-style is not described on the compatibility page; the preset uses it as the brief says [live]. If a real Space refuses it, flip the Spaces default in `endpoint()` (one line) and `bucket_url` already copes.
- Periods in a bucket name break the wildcard certificate for browser access [docs], which matches the fallback in `object_url`.
- Limits: new buckets 800 operations per second; older buckets 300 combined `PUT`/`POST`/`COPY`/`DELETE`/`LIST` per second, 10 concurrent `PUT`/`COPY` per object, 1,500 requests per second per IP. Respond to `503 Slow Down` with exponential backoff [docs]. This is far below the S3 transfer cap of 128; Task 9's adaptive backoff must see the 503.
- Multipart parts: at least 5 MiB except the last, up to 5 GB each, 10,000 parts, 5 TB total; incomplete uploads are deleted after 30 days; a single `PUT` up to 5 GB [docs].
- **Risk for Task 10:** the limits page also says "Multipart uploads and `PUT` requests sent to the CDN using presigned URLs have a maximum payload of 8100 KiB (7.91 MiB)". It does not say whether that is per part, and it contradicts the 5 GB part limit two sections later. If 16 MiB parts are refused on a Space, use 8 MiB parts for this preset [live].
- Supported: `CopyObject` (not across regions or clusters), multipart, `ListObjectsV2` and the older list call, SigV4 (recommended) and SigV2; unsupported calls return `NotImplemented` [docs].
- `PutObject`, `DeleteObjects`, `HeadObject`, `ListBuckets`, checksums and ETag are not covered on the page read [live].

## MinIO

- Path style is the default. Virtual-hosted needs `MINIO_DOMAIN`, a wildcard DNS record, a wildcard TLS certificate and Host forwarding, and then every sub-domain of that name is a bucket [docs]. The preset is therefore path style.
- Typically plain HTTP on a port (`http://localhost:9000`), so the address is typed in full.
- The region is configurable (`MINIO_SITE_REGION`); clients default to `us-east-1` [docs]. The preset signs with `us-east-1` unless another region is typed. A server with a different region configured answers `AuthorizationHeaderMalformed` [known].
- MinIO is the local target for S3 behaviour tests in the spec's test strategy; it is optional and the user is asked before its binary is downloaded.

## Other S3 and IP addresses

- The typed address is `http://` or `https://`, a host and an optional port, nothing else: no user name or password (they would turn into a Basic header), no path or query (a bucket typed here is the commonest mistake), no fragment.
- With the toggle off the bucket goes in the host (`bucket.host`). An IP address (`http://127.0.0.1:9000`, `http://[::1]:9000`) has no sub-domains, so `object_url` addresses by path whatever the toggle says.
- Bunny's own S3 endpoint (spec section 5.3) has no preset in `S3Preset`; it is reachable only through "Other S3" and untested.

## What this means for neighbouring tasks

- Task 9 (provider): map `AuthorizationHeaderMalformed` to the Region field, `SignatureDoesNotMatch` to the Secret field; send neither tagging nor `x-amz-checksum-*`; read `Retry-After` and treat `503 SlowDown` as rate limiting on Spaces and R2.
- Task 9: a folder marker needs a trailing slash on the object URL path; `object_url` never adds one.
- Task 10 (multipart): equal-size parts (R2), resend a failed part rather than trust the earlier copy (R2), check the 8100 KiB question on Spaces.
- Task 11 (live R2): confirm ETag format, `DeleteObjects` limit, `If-None-Match` behaviour and path-style addressing against the real account.
- Task 21 (UI): show the preview from `bucket_url(&endpoint(...), bucket)`; the error strings from `endpoint()` are written for the form fields.
