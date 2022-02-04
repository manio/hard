use futures::ready;
use std::io::{self, Error, ErrorKind, Read, Write};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::unix::AsyncFd;
use tokio::io::AsyncWrite;

pub struct AsyncFile {
    inner: AsyncFd<std::fs::File>,
}

impl AsyncFile {
    pub fn new(f: tokio::fs::File) -> io::Result<Self> {
        match f.try_into_std() {
            Ok(file) => Ok(Self {
                inner: AsyncFd::new(file)?,
            }),
            Err(_) => Err(Error::new(
                ErrorKind::Other,
                "Cannot destructure input file handle",
            )),
        }
    }

    pub async fn _read(&self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.inner.readable().await?;
            match guard.try_io(|inner| {
                let res = inner.get_ref().read(out);

                //handle Ok(0) results:
                if let Ok(len) = res {
                    if len == 0 {
                        return Err(Error::new(ErrorKind::Other, "USB disconnected"));
                    }
                }

                res
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }

    pub async fn read_exact(&self, mut out: &mut [u8]) -> io::Result<()> {
        while !out.is_empty() {
            let len = self._read(out).await?;
            out = &mut out[len..];
        }
        Ok(())
    }
}

impl AsyncWrite for AsyncFile {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.inner.poll_write_ready(cx))?;

            match guard.try_io(|inner| inner.get_ref().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
