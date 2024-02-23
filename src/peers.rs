
use std::net::SocketAddrV4;

use bytes::{Buf, BufMut};
use tokio::net::TcpStream;
use tokio_util::codec::{Decoder, Encoder, Framed};

pub(crate) struct Peers{
   addr: SocketAddrV4, 
   stream: Framed<TcpStream, MessageFramer>,
   bitfiled: Bitfield,
   choked: bool
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageTag {
    Choke = 0,
    Unchoke = 1,
    Interested = 2,
    NotInterested = 3,
    Have = 4,
    Bitfield = 5,
    Request = 6,
    Piece = 7,
    Cancel = 8,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub tag: MessageTag,
    pub payload: Vec<u8>,
}

pub struct MessageFramer;

const MAX : usize = 2<<16;

impl Decoder for MessageFramer {
    type Item = Message;

    type Error = std::io::Error;

    fn decode(&mut self, src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
      if src.len() < 4 {
         return Ok(None);
      }

      let mut length_bytes = [0u8; 4];
      length_bytes.copy_from_slice(&src[..4]);
      let length = u32::from_be_bytes(length_bytes) as usize;

      if length == 0 {
         // cut 4 bites
         src.advance(4);
         return self.decode(src);
      }

      if src.len() < 5 {
         return Ok(None);
      }

      if length > MAX {
         return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too long"));
      }

      if src.len() < length + 4 {
         // The full string has not yet arrived.
            //
            // We reserve more space in the buffer. This is not strictly
            // necessary, but is a good idea performance-wise.
            src.reserve(4 + length - src.len());

            return Ok(None);
      }

      let tag = match src[4] {
         0 => MessageTag::Choke,
         1 => MessageTag::Unchoke,
         2 => MessageTag::Interested,
         3 => MessageTag::NotInterested,
         4 => MessageTag::Have,
         5 => MessageTag::Bitfield,
         6 => MessageTag::Request,
         7 => MessageTag::Piece,
         8 => MessageTag::Cancel,
         tag => {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("invalid message tag: {}", tag)));
         }
      }; 

      let data = if src.len() > 5 {
         src[5..4+length].to_vec()
      } else {
         Vec::new()
      };

      src.advance(4 + length);

      Ok(Some(Message { tag, payload: data }))

    }
}

impl Encoder<Message> for MessageFramer {
    type Error = std::io::Error;

    fn encode(&mut self, item: Message, dst: &mut bytes::BytesMut) -> Result<(), Self::Error> {
      if item.payload.len() + 1 > MAX {
         return Err(std::io::Error::new(
             std::io::ErrorKind::InvalidData,
             format!("Frame of length {} is too large.", item.payload.len()),
         ));
     }

     // Convert the length into a byte array.
     let len_slice = u32::to_be_bytes(item.payload.len() as u32 + 1);

     // Reserve space in the buffer.
     dst.reserve(4 /* length */ + 1 /* tag */ + item.payload.len());

     // Write the length and string to the buffer.
     dst.extend_from_slice(&len_slice);
     dst.put_u8(item.tag as u8);
     dst.extend_from_slice(&item.payload);
     Ok(())
    }    
}

pub struct Bitfield {
    payload: Vec<u8>,
}

impl Bitfield {
    pub(crate) fn has_piece(&self, piece_i: usize) -> bool {
        let byte_i = piece_i / (u8::BITS as usize);
        let bit_i = (piece_i % (u8::BITS as usize)) as u32;
        let Some(&byte) = self.payload.get(byte_i) else {
            return false;
        };
        byte & 1u8.rotate_right(bit_i + 1) != 0
    }

    pub(crate) fn pieces(&self) -> impl Iterator<Item = usize> + '_ {
        self.payload.iter().enumerate().flat_map(|(byte_i, byte)| {
            (0..u8::BITS).filter_map(move |bit_i| {
                let piece_i = byte_i * (u8::BITS as usize) + (bit_i as usize);
                let mask = 1u8.rotate_right(bit_i + 1);
                (byte & mask != 0).then_some(piece_i)
            })
        })
    }

    fn from_payload(payload: Vec<u8>) -> Bitfield {
        Self { payload }
    }
}