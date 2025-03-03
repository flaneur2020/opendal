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
use std::task::Context;
use std::task::Poll;

use bb8::PooledConnection;
use futures::future::BoxFuture;
use futures::ready;
use futures::AsyncRead;
use futures::FutureExt;
use suppaftp::Status;

use super::backend::Manager;
use crate::raw::*;
use crate::Result;

/// Wrapper for ftp data stream and command stream.
pub struct FtpReader {
    reader: BytesReader,
    state: State,
}

unsafe impl Sync for FtpReader {}

pub enum State {
    Reading(Option<PooledConnection<'static, Manager>>),
    Finalize(BoxFuture<'static, Result<()>>),
}

impl FtpReader {
    /// Create an instance of FtpReader.
    pub fn new(r: BytesReader, c: PooledConnection<'static, Manager>) -> Self {
        Self {
            reader: r,
            state: State::Reading(Some(c)),
        }
    }
}

impl OutputBytesRead for FtpReader {
    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut [u8]) -> Poll<io::Result<usize>> {
        let data = Pin::new(&mut self.reader).poll_read(cx, buf);

        match &mut self.state {
            // Reading state, try to poll some data.
            State::Reading(stream) => {
                match stream {
                    Some(_) => {
                        // when hit Err or EOF, consume ftpstream, change state to Finalize and send fut.
                        if let Poll::Ready(Err(_)) | Poll::Ready(Ok(0)) = data {
                            let mut ft = stream.take().unwrap();

                            let fut = async move {
                                ft.read_response_in(&[
                                    Status::ClosingDataConnection,
                                    Status::RequestedFileActionOk,
                                ])
                                .await?;

                                Ok(())
                            };
                            self.state = State::Finalize(Box::pin(fut));
                        } else {
                            // Otherwise, exit and return data.
                            return data;
                        }

                        self.poll_read(cx, buf)
                    }
                    // We could never reach this branch because we will change to Finalize state once we consume ftp stream.
                    None => unreachable!(),
                }
            }

            // Finalize state, wait for finalization of stream.
            State::Finalize(fut) => match ready!(Pin::new(fut).poll_unpin(cx)) {
                Ok(_) => Poll::Ready(Ok(0)),
                Err(e) => Poll::Ready(Err(e.into())),
            },
        }
    }
}
