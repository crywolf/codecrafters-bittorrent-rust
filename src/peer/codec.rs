use bytes::{Buf, BufMut, BytesMut};
use tokio_util::codec::Decoder;
use tokio_util::codec::Encoder;

use super::download::{Message, MessageTag};

pub struct MessageCodec;

const MAX: usize = (1 << 14) + 1000; // 2^14 (16 kiB) + some reserve

impl Decoder for MessageCodec {
    type Item = Message;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < 4 {
            // Not enough data to read length marker.
            return Ok(None);
        }

        // Read length marker.
        let mut length_bytes = [0u8; 4];
        length_bytes.copy_from_slice(&src[..4]);
        let length = u32::from_be_bytes(length_bytes) as usize;

        if length == 0 {
            // keepalive message => ignore it for now and discard its content
            // and immediately continue reading another message
            src.advance(4);
            return self.decode(src);
        }

        if src.len() < 5 {
            // Not enough data to read tag marker
            return Ok(None);
        }

        // Check that the length is not too large to avoid a denial of
        // service attack where the server runs out of memory.
        if length > MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Frame of length {} is too large.", length),
            ));
        }

        if src.len() < 4 + length {
            // The full message has not yet arrived.
            //
            // We reserve more space in the buffer. This is not strictly
            // necessary, but is a good idea performance-wise.
            src.reserve(4 + length - src.len());

            // We inform the Framed that we need more bytes to form the next frame.
            return Ok(None);
        }

        src.advance(4); // we have alredady proccessed legnth marker => remove it from src

        // Read message tag
        let tag = src.get_u8();

        let tag: MessageTag = match MessageTag::try_from(tag) {
            Ok(tag) => tag,
            Err(_) => {
                // unknown message => ignore it for now and discard its content
                // and immediately continue reading another message
                src.advance(length - 1); // we have already removed tag marker from src
                return self.decode(src);
            }
        };

        // Use advance to modify src such that it no longer contains this frame.
        let data = src[..length - 1].to_vec(); // length includes tag which we already removed from src
        src.advance(length - 1);

        Ok(Some(Message { tag, payload: data }))
    }
}

impl Encoder<Message> for MessageCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let payload_len = item.payload.len() + 1; // payload + tag

        // Don't send a payload if it is longer than the other end will accept.
        if payload_len > MAX {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Frame of length {} is too large.", payload_len),
            ));
        }

        // Convert the length into a byte array.
        // The cast to u32 cannot overflow due to the length check above.
        let len_slice = u32::to_be_bytes(payload_len as u32);

        // Reserve space in the buffer.
        dst.reserve(4 + payload_len);

        // Write the length and string to the buffer.
        dst.extend_from_slice(&len_slice);
        dst.put_u8(item.tag.into());

        dst.extend_from_slice(&item.payload);
        Ok(())
    }
}
