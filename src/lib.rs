mod split;

use bytes::Bytes;
use futures::{FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use rusoto_core::{ByteStream, RusotoError};
use rusoto_s3::{
    AbortMultipartUploadRequest, CompleteMultipartUploadError, CompleteMultipartUploadOutput,
    CompleteMultipartUploadRequest, CompletedMultipartUpload, CompletedPart,
    CreateMultipartUploadError, CreateMultipartUploadRequest, UploadPartError, UploadPartRequest,
    S3,
};
use std::future::Future;
use std::ops::RangeInclusive;
use std::task::Poll;

// https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html
pub const PART_SIZE: RangeInclusive<usize> = 5 << 20..=5 << 30;

pub struct MultipartUploadRequest<B, E>
where
    B: Stream<Item = Result<Bytes, E>>,
{
    pub body: B,
    pub bucket: String,
    pub key: String,
}

pub type MultipartUploadOutput = CompleteMultipartUploadOutput;

pub async fn multipart_upload<C, B, E>(
    client: &C,
    input: MultipartUploadRequest<B, E>,
    part_size: RangeInclusive<usize>,
    concurrency_limit: Option<usize>,
) -> Result<MultipartUploadOutput, E>
where
    C: S3,
    B: Stream<Item = Result<Bytes, E>>,
    E: From<RusotoError<CreateMultipartUploadError>>
        + From<RusotoError<UploadPartError>>
        + From<RusotoError<CompleteMultipartUploadError>>,
{
    let MultipartUploadRequest { body, bucket, key } = input;

    let output = client
        .create_multipart_upload(CreateMultipartUploadRequest {
            bucket: bucket.clone(),
            key: key.clone(),
            ..CreateMultipartUploadRequest::default()
        })
        .await?;
    let upload_id = output.upload_id.as_ref().unwrap();

    let futures = split::split(body, part_size).map_ok(|part| {
        client
            .upload_part(UploadPartRequest {
                body: Some(ByteStream::new(futures::stream::iter(
                    part.body.into_iter().map(Ok),
                ))),
                bucket: bucket.clone(),
                content_length: Some(part.content_length as _),
                content_md5: Some(base64::encode(part.content_md5)),
                key: key.clone(),
                part_number: part.part_number as _,
                upload_id: upload_id.clone(),
                ..UploadPartRequest::default()
            })
            .map_ok({
                let part_number = part.part_number;
                move |output| CompletedPart {
                    e_tag: output.e_tag,
                    part_number: Some(part_number as _),
                }
            })
            .err_into()
    });

    (async {
        let mut completed_parts = dispatch_concurrent(futures, concurrency_limit).await?;
        completed_parts.sort_by_key(|completed_part| completed_part.part_number);

        let output = client
            .complete_multipart_upload(CompleteMultipartUploadRequest {
                bucket: bucket.clone(),
                key: key.clone(),
                multipart_upload: Some(CompletedMultipartUpload {
                    parts: Some(completed_parts),
                }),
                upload_id: upload_id.clone(),
                ..CompleteMultipartUploadRequest::default()
            })
            .await?;

        Ok(output)
    })
    .or_else(|e| {
        client
            .abort_multipart_upload(AbortMultipartUploadRequest {
                bucket: bucket.clone(),
                key: key.clone(),
                upload_id: upload_id.clone(),
                ..AbortMultipartUploadRequest::default()
            })
            .map(|_| Err(e))
    })
    .await
}

async fn dispatch_concurrent<S, F, T, E>(stream: S, limit: Option<usize>) -> Result<Vec<T>, E>
where
    S: Stream<Item = Result<F, E>>,
    F: Future<Output = Result<T, E>> + Unpin,
{
    futures::pin_mut!(stream);

    if let Some(limit) = limit {
        assert!(limit > 0);
    }

    let mut stream = stream.fuse();
    let mut futures = Vec::new();
    let mut outputs = Vec::new();

    futures::future::poll_fn(|cx| {
        while !stream.is_done() || !futures.is_empty() {
            let mut is_pending = false;
            while limit.map_or(true, |limit| limit > futures.len()) {
                match stream.poll_next_unpin(cx) {
                    Poll::Ready(Some(Ok(future))) => futures.push(future),
                    Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                    Poll::Ready(None) => break,
                    Poll::Pending => {
                        is_pending = true;
                        break;
                    }
                }
            }
            let mut i = 0;
            while i < futures.len() {
                match futures[i].poll_unpin(cx) {
                    Poll::Ready(Ok(output)) => {
                        futures.swap_remove(i);
                        outputs.push(output);
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => {
                        is_pending = true;
                        i += 1;
                    }
                }
            }
            if is_pending {
                return Poll::Pending;
            }
        }
        Poll::Ready(Ok(()))
    })
    .await?;

    Ok(outputs)
}

#[cfg(test)]
mod tests;
