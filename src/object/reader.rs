// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use bytes::Bytes;
use futures::AsyncRead;
use futures::AsyncSeek;
use futures::Stream;
use parking_lot::Mutex;

use crate::error::Result;
use crate::raw::*;
use crate::ObjectMetadata;
use crate::OpRead;
use crate::OpStat;

/// ObjectReader is the public API for users.
///
/// # Usage
///
/// ObjectReader implements the following APIs:
///
/// - `AsyncRead`
/// - `AsyncSeek`
/// - `Stream<Item = <io::Result<Bytes>>>`
///
/// For reading data, we can use `AsyncRead` and `Stream`. The mainly
/// different is where the `copy` happend.
///
/// `AsyncRead` requires user to prepare a buffer for `ObjectReader` to fill.
/// And `Stream` will stream out a `Bytes` for user to decide when to copy
/// it's content.
///
/// For example, users may have their only CPU/IO bound workers and don't
/// want to do copy inside IO workers. They can use `Stream` to get a `Bytes`
/// and consume it in side CPU workers inside.
///
/// Besides, `Stream` **COULD** reduce an extra copy if underlying reader is
/// stream based (like services s3, azure which based on HTTP).
///
/// # Notes
///
/// All implementions of ObjectReader should be `zero cost`. In our cases,
/// which means others must pay the same cost for the same feature provide
/// by us.
///
/// For examples, call `read` without `seek` should always act the same as
/// calling `read` on plain reader.
///
/// ## Read is Seekable
///
/// We use internal `AccessorHint::ReadIsSeekable` to decide the most
/// suitable implementations.
///
/// If there is a hint that `ReadIsSeekable`, we will open it with given args
/// directy. Otherwise, we will pick a seekable reader implementation based
/// on input range for it.
///
/// - `Some(offset), Some(size)` => `RangeReader`
/// - `Some(offset), None` and `None, None` => `OffsetReader`
/// - `None, Some(size)` => get the total size first to convert as `RangeReader`
///
/// No matter which reader we use, we will make sure the `read` operation
/// is zero cost.
///
/// ## Read is Streamable
///
/// We use internal `AccessorHint::ReadIsStreamable` to decide the most
/// suitable implementations.
///
/// If there is a hint that `ReadIsStreamable`, we will use existing reader
/// directly. Otherwise, we will use transform this reader as a stream.
///
/// ## Consume instead of Drop
///
/// Normally, if reader is seekable, we need to drop current reader and start
/// a new read call.
///
/// We can consume the data if the seek position is close enough. For
/// example, users try to seek to `Current(1)`, we can just read the data
/// can consume it.
///
/// In this way, we can reduce the extra cost of dropping reader.
pub struct ObjectReader {
    inner: OutputBytesReader,
}

impl ObjectReader {
    /// Create a new object reader.
    ///
    /// Create will use internal information to decide the most suitable
    /// implementaion for users.
    ///
    /// We don't want to expose those detials to users so keep this fuction
    /// in crate only.
    pub(crate) async fn create(
        acc: Arc<dyn Accessor>,
        path: &str,
        meta: Arc<Mutex<ObjectMetadata>>,
        op: OpRead,
    ) -> Result<Self> {
        let acc_meta = acc.metadata();

        let r = if acc_meta.hints().contains(AccessorHint::ReadIsSeekable) {
            let (_, r) = acc.read(path, op).await?;
            r
        } else {
            match (op.range().offset(), op.range().size()) {
                (Some(offset), Some(size)) => {
                    Box::new(into_seekable_reader::by_range(acc, path, offset, size))
                        as OutputBytesReader
                }
                (Some(offset), None) => {
                    Box::new(into_seekable_reader::by_offset(acc, path, offset))
                }
                (None, Some(size)) => {
                    let total_size = get_total_size(acc.clone(), path, meta).await?;
                    let (offset, size) = if size > total_size {
                        (0, total_size)
                    } else {
                        (total_size - size, size)
                    };

                    Box::new(into_seekable_reader::by_range(acc, path, offset, size))
                }
                (None, None) => Box::new(into_seekable_reader::by_offset(acc, path, 0)),
            }
        };

        let r = if acc_meta.hints().contains(AccessorHint::ReadIsStreamable) {
            r
        } else {
            // Make this capacity configurable.
            Box::new(into_seekable_stream(r, 256 * 1024))
        };

        Ok(ObjectReader { inner: r })
    }
}

impl OutputBytesRead for ObjectReader {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        self.inner.poll_read(cx, buf)
    }

    fn poll_seek(&mut self, cx: &mut Context<'_>, pos: io::SeekFrom) -> Poll<io::Result<u64>> {
        self.inner.poll_seek(cx, pos)
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>> {
        self.inner.poll_next(cx)
    }
}

impl AsyncRead for ObjectReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncSeek for ObjectReader {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: io::SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Pin::new(&mut self.inner).poll_seek(cx, pos)
    }
}

impl Stream for ObjectReader {
    type Item = io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// get_total_size will get total size via stat.
async fn get_total_size(
    acc: Arc<dyn Accessor>,
    path: &str,
    meta: Arc<Mutex<ObjectMetadata>>,
) -> Result<u64> {
    if let Some(v) = meta.lock().content_length_raw() {
        return Ok(v);
    }

    let om = acc.stat(path, OpStat::new()).await?.into_metadata();
    let size = om.content_length();
    *(meta.lock()) = om;
    Ok(size)
}
