//! Lossless bounded framing. No authentication or row payload reserialization.
use bytes::{Bytes, BytesMut};
use std::io::{self, ErrorKind};
use tokio::io::{AsyncRead, AsyncReadExt};
pub const MAX_FRAME: usize = 64 * 1024 * 1024;
pub const SSL_REQUEST: u32 = 80877103;
pub const GSS_REQUEST: u32 = 80877104;
pub const CANCEL_REQUEST: u32 = 80877102;
pub const PROTOCOL_V3: u32 = 196608;

#[derive(Clone, Debug)]
pub struct Frame(pub Bytes);
impl Frame {
    pub fn tag(&self) -> u8 {
        self.0[0]
    }
    pub fn body(&self) -> &[u8] {
        &self.0[5..]
    }
}
fn invalid(s: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, s)
}

/// Buffer survives cancellation of `next`, including a partial TCP read.
#[derive(Default)]
pub struct FrameReader {
    buffer: BytesMut,
}
impl FrameReader {
    pub async fn next<R: AsyncRead + Unpin>(&mut self, r: &mut R) -> io::Result<Option<Frame>> {
        loop {
            if let Some(frame) = decode(&mut self.buffer)? {
                return Ok(Some(frame));
            }
            if r.read_buf(&mut self.buffer).await? == 0 {
                return if self.buffer.is_empty() {
                    Ok(None)
                } else {
                    Err(invalid("EOF inside PostgreSQL frame"))
                };
            }
        }
    }
}
pub fn decode(buf: &mut BytesMut) -> io::Result<Option<Frame>> {
    if buf.len() < 5 {
        return Ok(None);
    }
    let len = u32::from_be_bytes(buf[1..5].try_into().unwrap()) as usize;
    if !(4..=MAX_FRAME).contains(&len) {
        return Err(invalid("invalid PostgreSQL frame length"));
    }
    if buf.len() < len + 1 {
        return Ok(None);
    }
    Ok(Some(Frame(buf.split_to(len + 1).freeze())))
}
pub async fn startup<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Bytes> {
    let len = r.read_u32().await? as usize;
    if !(8..=1024 * 1024).contains(&len) {
        return Err(invalid("invalid startup length"));
    }
    let mut data = vec![0; len];
    data[..4].copy_from_slice(&(len as u32).to_be_bytes());
    r.read_exact(&mut data[4..]).await?;
    Ok(data.into())
}
pub fn cstr(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = body.iter().position(|b| *b == 0)?;
    Some((&body[..end], &body[end + 1..]))
}
pub fn text(body: &[u8]) -> Option<String> {
    Some(String::from_utf8_lossy(cstr(body)?.0).into_owned())
}
pub fn frame(tag: u8, body: &[u8]) -> Frame {
    let mut bytes = Vec::with_capacity(body.len() + 5);
    bytes.push(tag);
    bytes.extend_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    Frame(bytes.into())
}
#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn fragmentation_preserves_raw_bytes(payload in prop::collection::vec(any::<u8>(), 0..4096), chunks in prop::collection::vec(1usize..100, 1..100)) {
            let original = frame(b'?', &payload);
            let mut stream = original.0.to_vec(); stream.extend_from_slice(&original.0);
            let mut buf = BytesMut::new(); let mut out = Vec::new(); let mut offset = 0; let mut i = 0;
            while offset < stream.len() {
                let end = (offset + chunks[i % chunks.len()]).min(stream.len());
                buf.extend_from_slice(&stream[offset..end]); offset=end; i+=1;
                while let Some(f) = decode(&mut buf).unwrap() { out.push(f.0); }
            }
            prop_assert_eq!(out, vec![original.0.clone(), original.0]); prop_assert!(buf.is_empty());
        }
    }
    #[test]
    fn rejects_lengths() {
        for n in [0u32, 3, MAX_FRAME as u32 + 1, u32::MAX] {
            let mut b = BytesMut::from(&[b'Q'][..]);
            b.extend_from_slice(&n.to_be_bytes());
            assert!(decode(&mut b).is_err());
        }
    }
}
