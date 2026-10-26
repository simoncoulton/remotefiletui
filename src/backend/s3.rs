//! Amazon S3 and S3-compatible object stores (MinIO, Cloudflare R2, Wasabi,
//! Backblaze B2, …) via the AWS SDK.
//!
//! Object stores have no real directories. A "directory" here is a key prefix
//! ending in `/`, listed with the `/` delimiter; `mkdir` creates the usual
//! zero-byte `prefix/` placeholder object. Paths look like `/bucket/dir/file`,
//! or `/dir/file` when the connection is pinned to a single bucket.

use std::{
    path::Path,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region, timeout::TimeoutConfig};
use aws_sdk_s3::{
    Client,
    config::Credentials,
    error::{DisplayErrorContext, ProvideErrorMetadata, SdkError},
    primitives::ByteStream,
    types::{
        BucketLocationConstraint, CompletedMultipartUpload, CompletedPart,
        CreateBucketConfiguration,
    },
};
use tokio::io::AsyncReadExt;

use super::{
    Backend, Cancelled, ConnectParams, Entry, EntryKind, TransferCtl, copy_with_progress,
    finalize_download, part_path,
};
use crate::util::is_listable;

/// Smallest part S3 accepts for multipart uploads is 5 MiB; 8 MiB keeps requests few.
const MIN_PART: u64 = 8 * 1024 * 1024;
const MAX_PARTS: u64 = 9_000;

/// Turn an SDK error into something a person can act on. The SDK's own
/// `Display` for service errors is a debug dump when S3 sends no message
/// (HEAD requests never do).
fn describe<E>(e: &SdkError<E>) -> String
where
    E: std::error::Error + ProvideErrorMetadata + 'static,
{
    match e {
        SdkError::ServiceError(se) => {
            let status = se.raw().status().as_u16();
            let (code, message) = (e.code(), e.message());
            match (code, status) {
                (Some("NoSuchBucket"), _) | (Some("NotFound"), 404) | (None, 404) => {
                    "not found: the bucket or object does not exist, or the endpoint/region is wrong"
                        .to_string()
                }
                (Some("NoSuchKey"), _) => "no such object".to_string(),
                (Some("AccessDenied"), _) | (None, 403) => {
                    "access denied: check the access keys and the bucket policy".to_string()
                }
                (Some("InvalidAccessKeyId"), _) => {
                    "the access key id is not recognised by this endpoint".to_string()
                }
                (Some("SignatureDoesNotMatch"), _) => {
                    "the secret key is wrong (signature mismatch)".to_string()
                }
                (Some("BucketNotEmpty"), _) => "the bucket is not empty".to_string(),
                (Some("BucketAlreadyOwnedByYou" | "BucketAlreadyExists"), _) => {
                    "a bucket with that name already exists".to_string()
                }
                (Some(c), _) => match message {
                    Some(m) => format!("{c}: {m}"),
                    None => format!("{c} (HTTP {status})"),
                },
                (None, _) => message.map_or_else(|| format!("HTTP {status}"), str::to_string),
            }
        }
        SdkError::DispatchFailure(df) if df.is_timeout() => {
            "timed out connecting to the endpoint".to_string()
        }
        SdkError::DispatchFailure(_) => {
            "cannot reach the endpoint (is the URL right, and is it up?)".to_string()
        }
        SdkError::TimeoutError(_) => "the request timed out".to_string(),
        other => DisplayErrorContext(other).to_string(),
    }
}

fn err<E>(e: SdkError<E>) -> anyhow::Error
where
    E: std::error::Error + ProvideErrorMetadata + 'static,
{
    anyhow!("{}", describe(&e))
}

/// Where a path points: nowhere in particular (the list of buckets), or into a bucket.
#[derive(Debug, PartialEq, Eq)]
enum Loc {
    /// The list of buckets (only when the connection isn't pinned to one).
    Buckets,
    Object {
        bucket: String,
        key: String,
    },
}

fn locate(fixed_bucket: Option<&str>, path: &str) -> Loc {
    let trimmed = path.trim_matches('/');
    match fixed_bucket {
        Some(b) => Loc::Object {
            bucket: b.to_string(),
            key: trimmed.to_string(),
        },
        None if trimmed.is_empty() => Loc::Buckets,
        None => match trimmed.split_once('/') {
            Some((bucket, key)) => Loc::Object {
                bucket: bucket.to_string(),
                key: key.to_string(),
            },
            None => Loc::Object {
                bucket: trimmed.to_string(),
                key: String::new(),
            },
        },
    }
}

/// A listing prefix: empty, or ending in `/`.
fn as_prefix(key: &str) -> String {
    let k = key.trim_end_matches('/');
    if k.is_empty() {
        String::new()
    } else {
        format!("{k}/")
    }
}

/// Percent-encode an `x-amz-copy-source` value (`bucket/key`), keeping `/`.
fn encode_copy_source(bucket: &str, key: &str) -> String {
    let mut out = String::new();
    for b in format!("{bucket}/{key}").bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn endpoint_url(raw: &str) -> String {
    let raw = raw.trim();
    if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    }
}

async fn build_client(p: &ConnectParams, region: Option<String>) -> Client {
    let mut loader = aws_config::defaults(BehaviorVersion::latest()).timeout_config(
        TimeoutConfig::builder()
            .connect_timeout(Duration::from_secs(15))
            .build(),
    );
    if !p.aws_profile.is_empty() {
        loader = loader.profile_name(&p.aws_profile);
    }
    if let Some(r) = region.or_else(|| (!p.region.is_empty()).then(|| p.region.clone())) {
        loader = loader.region(Region::new(r));
    }
    // Explicit keys win; otherwise the default chain (env, ~/.aws, SSO, IMDS…).
    if !p.username.is_empty() {
        loader = loader.credentials_provider(Credentials::new(
            p.username.clone(),
            p.password.clone(),
            None,
            None,
            "remotefiletui",
        ));
    }
    let sdk = loader.load().await;
    let mut conf = aws_sdk_s3::config::Builder::from(&sdk);
    if sdk.region().is_none() {
        conf = conf.region(Region::new("us-east-1"));
    }
    if !p.host.trim().is_empty() {
        // Custom endpoints (MinIO etc.) generally need path-style addressing.
        conf = conf
            .endpoint_url(endpoint_url(&p.host))
            .force_path_style(true);
    }
    Client::from_conf(conf.build())
}

pub async fn connect(p: ConnectParams) -> Result<Box<dyn Backend>> {
    if p.username.is_empty() != p.password.is_empty() {
        bail!("enter both the access key id and the secret key, or leave both empty");
    }
    let fixed = (!p.bucket.trim().is_empty()).then(|| p.bucket.trim().to_string());
    let mut client = build_client(&p, None).await;
    let mut region = p.region.clone();

    match &fixed {
        Some(bucket) => {
            let mut result = client.head_bucket().bucket(bucket).send().await;
            // A bucket in another region answers with its region in a header.
            if let Err(SdkError::ServiceError(se)) = &result
                && let Some(r) = se.raw().headers().get("x-amz-bucket-region")
                && r != region
            {
                region = r.to_string();
                client = build_client(&p, Some(region.clone())).await;
                result = client.head_bucket().bucket(bucket).send().await;
            }
            result.map_err(|e| anyhow!("cannot access bucket '{bucket}': {}", describe(&e)))?;
        }
        None => {
            client.list_buckets().send().await.map_err(|e| {
                anyhow!(
                    "cannot list buckets: {} (if these credentials can't list buckets, \
                     enter a bucket name)",
                    describe(&e)
                )
            })?;
        }
    }
    Ok(Box::new(S3Backend {
        client,
        fixed,
        region,
    }))
}

struct S3Backend {
    client: Client,
    fixed: Option<String>,
    region: String,
}

impl S3Backend {
    fn loc(&self, path: &str) -> Loc {
        locate(self.fixed.as_deref(), path)
    }

    /// Bucket and key of an object path; fails for the bucket list or a bare bucket.
    fn object(&self, path: &str) -> Result<(String, String)> {
        match self.loc(path) {
            Loc::Object { bucket, key } if !key.is_empty() => Ok((bucket, key)),
            _ => bail!("'{path}' is not an object"),
        }
    }
}

#[async_trait]
impl Backend for S3Backend {
    async fn home(&mut self) -> Result<String> {
        Ok("/".into())
    }

    async fn list(&mut self, path: &str) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        match self.loc(path) {
            Loc::Buckets => {
                let resp = self.client.list_buckets().send().await.map_err(err)?;
                for b in resp.buckets() {
                    let Some(name) = b.name().filter(|n| is_listable(n)) else {
                        continue;
                    };
                    out.push(Entry {
                        name: name.to_string(),
                        kind: EntryKind::Dir,
                        is_link: false,
                        size: 0,
                        modified: b
                            .creation_date()
                            .and_then(|d| SystemTime::try_from(*d).ok()),
                        mode: None,
                    });
                }
            }
            Loc::Object { bucket, key } => {
                let prefix = as_prefix(&key);
                let mut token: Option<String> = None;
                loop {
                    let resp = self
                        .client
                        .list_objects_v2()
                        .bucket(&bucket)
                        .prefix(&prefix)
                        .delimiter("/")
                        .set_continuation_token(token.take())
                        .send()
                        .await
                        .map_err(err)?;
                    for cp in resp.common_prefixes() {
                        let Some(full) = cp.prefix() else { continue };
                        let name = full
                            .strip_prefix(&prefix)
                            .unwrap_or(full)
                            .trim_end_matches('/');
                        if is_listable(name) {
                            out.push(Entry {
                                name: name.to_string(),
                                kind: EntryKind::Dir,
                                is_link: false,
                                size: 0,
                                modified: None,
                                mode: None,
                            });
                        }
                    }
                    for obj in resp.contents() {
                        let Some(k) = obj.key() else { continue };
                        let name = k.strip_prefix(&prefix).unwrap_or(k);
                        // Skip the folder placeholder itself and other `…/` markers.
                        if name.is_empty() || name.ends_with('/') || !is_listable(name) {
                            continue;
                        }
                        out.push(Entry {
                            name: name.to_string(),
                            kind: EntryKind::File,
                            is_link: false,
                            size: obj.size().unwrap_or(0).max(0) as u64,
                            modified: obj
                                .last_modified()
                                .and_then(|d| SystemTime::try_from(*d).ok()),
                            mode: None,
                        });
                    }
                    match resp.next_continuation_token() {
                        Some(t) if resp.is_truncated().unwrap_or(false) => {
                            token = Some(t.to_string())
                        }
                        _ => break,
                    }
                }
            }
        }
        Ok(out)
    }

    async fn mkdir(&mut self, path: &str) -> Result<()> {
        match self.loc(path) {
            Loc::Buckets => bail!("choose a name for the new bucket"),
            Loc::Object { bucket, key } if key.is_empty() => {
                if self.fixed.is_some() {
                    bail!("the bucket root already exists");
                }
                let mut req = self.client.create_bucket().bucket(&bucket);
                // Outside us-east-1 S3 wants the region spelled out.
                if self.region != "us-east-1" && !self.region.is_empty() {
                    req = req.create_bucket_configuration(
                        CreateBucketConfiguration::builder()
                            .location_constraint(BucketLocationConstraint::from(
                                self.region.as_str(),
                            ))
                            .build(),
                    );
                }
                req.send().await.map_err(err)?;
            }
            Loc::Object { bucket, key } => {
                self.client
                    .put_object()
                    .bucket(bucket)
                    .key(as_prefix(&key))
                    .body(ByteStream::from_static(b""))
                    .send()
                    .await
                    .map_err(err)?;
            }
        }
        Ok(())
    }

    async fn remove_file(&mut self, path: &str) -> Result<()> {
        let (bucket, key) = self.object(path)?;
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(err)?;
        Ok(())
    }

    async fn remove_dir(&mut self, path: &str) -> Result<()> {
        match self.loc(path) {
            Loc::Buckets => bail!("not a directory"),
            Loc::Object { bucket, key } if key.is_empty() => {
                if self.fixed.is_some() {
                    bail!("cannot delete the bucket this connection is pinned to");
                }
                self.client
                    .delete_bucket()
                    .bucket(bucket)
                    .send()
                    .await
                    .map_err(err)?;
            }
            Loc::Object { bucket, key } => {
                let prefix = as_prefix(&key);
                let resp = self
                    .client
                    .list_objects_v2()
                    .bucket(&bucket)
                    .prefix(&prefix)
                    .max_keys(2)
                    .send()
                    .await
                    .map_err(err)?;
                if resp
                    .contents()
                    .iter()
                    .any(|o| o.key() != Some(prefix.as_str()))
                {
                    bail!("directory is not empty");
                }
                // Only the placeholder (if any) remains; deleting is idempotent.
                self.client
                    .delete_object()
                    .bucket(bucket)
                    .key(prefix)
                    .send()
                    .await
                    .map_err(err)?;
            }
        }
        Ok(())
    }

    async fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        // S3 has no rename: copy then delete.
        let (src_bucket, src_key) = self.object(from)?;
        let (dst_bucket, dst_key) = self.object(to)?;
        let copied = self
            .client
            .copy_object()
            .copy_source(encode_copy_source(&src_bucket, &src_key))
            .bucket(&dst_bucket)
            .key(&dst_key)
            .send()
            .await;
        match copied {
            Ok(_) => {}
            Err(SdkError::ServiceError(se)) if se.err().is_object_not_in_active_tier_error() => {
                bail!("the object is archived and must be restored first")
            }
            Err(e) if e.code() == Some("NoSuchKey") => {
                bail!("only files can be renamed in S3, not directories")
            }
            Err(e) => return Err(err(e)),
        }
        self.client
            .delete_object()
            .bucket(src_bucket)
            .key(src_key)
            .send()
            .await
            .map_err(err)?;
        Ok(())
    }

    async fn download(&mut self, remote: &str, local: &Path, ctl: &TransferCtl) -> Result<()> {
        let (bucket, key) = self.object(remote)?;
        let resp = self
            .client
            .get_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(err)?;
        ctl.set_total(resp.content_length().unwrap_or(0).max(0) as u64);
        let tmp = part_path(local);
        let mut dst = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        let mut body = std::pin::pin!(resp.body.into_async_read());
        let result = copy_with_progress(&mut body, &mut dst, ctl).await;
        drop(dst);
        finalize_download(result, &tmp, local).await
    }

    async fn upload(&mut self, local: &Path, remote: &str, ctl: &TransferCtl) -> Result<()> {
        let (bucket, key) = self.object(remote)?;
        let mut file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("cannot open {}", local.display()))?;
        let size = file.metadata().await?.len();
        ctl.set_total(size);

        if size <= MIN_PART {
            let mut data = Vec::with_capacity(size as usize);
            file.read_to_end(&mut data).await?;
            let put = self
                .client
                .put_object()
                .bucket(&bucket)
                .key(&key)
                .body(ByteStream::from(data))
                .send();
            tokio::select! {
                res = put => { res.map_err(err)?; }
                _ = ctl.cancelled() => return Err(Cancelled.into()),
            }
            ctl.add_done(size);
            return Ok(());
        }

        // Multipart: parts are read into memory one at a time, so progress and
        // cancellation are per part; an aborted upload is cleaned up server-side.
        let part_size = MIN_PART.max(size.div_ceil(MAX_PARTS));
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .map_err(err)?;
        let upload_id = created
            .upload_id()
            .ok_or_else(|| anyhow!("S3 did not return an upload id"))?
            .to_string();

        let result = self
            .upload_parts(&mut file, &bucket, &key, &upload_id, part_size, ctl)
            .await;
        match result {
            Ok(parts) => {
                let done = self
                    .client
                    .complete_multipart_upload()
                    .bucket(&bucket)
                    .key(&key)
                    .upload_id(&upload_id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(parts))
                            .build(),
                    )
                    .send()
                    .await;
                if let Err(e) = done {
                    self.abort_upload(&bucket, &key, &upload_id).await;
                    return Err(err(e));
                }
                Ok(())
            }
            Err(e) => {
                self.abort_upload(&bucket, &key, &upload_id).await;
                Err(e)
            }
        }
    }

    async fn close(&mut self) {}
}

impl S3Backend {
    async fn upload_parts(
        &self,
        file: &mut tokio::fs::File,
        bucket: &str,
        key: &str,
        upload_id: &str,
        part_size: u64,
        ctl: &TransferCtl,
    ) -> Result<Vec<CompletedPart>> {
        let mut parts = Vec::new();
        let mut number: i32 = 1;
        loop {
            if ctl.is_cancelled() {
                return Err(Cancelled.into());
            }
            let mut buf = Vec::with_capacity(part_size as usize);
            let n = file.take(part_size).read_to_end(&mut buf).await?;
            if n == 0 {
                break;
            }
            let sent = self
                .client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(number)
                .body(ByteStream::from(buf))
                .send();
            let resp = tokio::select! {
                res = sent => res.map_err(err)?,
                _ = ctl.cancelled() => return Err(Cancelled.into()),
            };
            parts.push(
                CompletedPart::builder()
                    .part_number(number)
                    .set_e_tag(resp.e_tag().map(str::to_string))
                    .build(),
            );
            ctl.add_done(n as u64);
            number += 1;
        }
        Ok(parts)
    }

    /// Best effort: an abandoned multipart upload would otherwise keep costing storage.
    async fn abort_upload(&self, bucket: &str, key: &str, upload_id: &str) {
        let _ = self
            .client
            .abort_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locates_paths_for_all_buckets() {
        assert_eq!(locate(None, "/"), Loc::Buckets);
        assert_eq!(
            locate(None, "/b"),
            Loc::Object {
                bucket: "b".into(),
                key: "".into()
            }
        );
        assert_eq!(
            locate(None, "/b/dir/file.txt"),
            Loc::Object {
                bucket: "b".into(),
                key: "dir/file.txt".into()
            }
        );
        assert_eq!(
            locate(None, "/b/dir/"),
            Loc::Object {
                bucket: "b".into(),
                key: "dir".into()
            }
        );
    }

    #[test]
    fn locates_paths_for_a_pinned_bucket() {
        assert_eq!(
            locate(Some("pin"), "/"),
            Loc::Object {
                bucket: "pin".into(),
                key: "".into()
            }
        );
        assert_eq!(
            locate(Some("pin"), "/a/b"),
            Loc::Object {
                bucket: "pin".into(),
                key: "a/b".into()
            }
        );
    }

    #[test]
    fn prefixes() {
        assert_eq!(as_prefix(""), "");
        assert_eq!(as_prefix("a"), "a/");
        assert_eq!(as_prefix("a/b/"), "a/b/");
    }

    #[test]
    fn copy_source_encoding() {
        assert_eq!(encode_copy_source("b", "a b/c+d.txt"), "b/a%20b/c%2Bd.txt");
        assert_eq!(encode_copy_source("b", "ü"), "b/%C3%BC");
        assert_eq!(encode_copy_source("b", "plain-key_1.~"), "b/plain-key_1.~");
    }

    #[test]
    fn endpoint_normalisation() {
        assert_eq!(endpoint_url("minio.local:9000"), "https://minio.local:9000");
        assert_eq!(
            endpoint_url(" http://127.0.0.1:5000 "),
            "http://127.0.0.1:5000"
        );
    }
}
