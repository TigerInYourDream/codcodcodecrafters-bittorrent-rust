use crate::BLOCK_MAX;
use anyhow::Context;
use bytes::{Buf, BufMut};
use futures_util::{SinkExt, StreamExt};
use std::{mem, net::SocketAddrV4};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tokio_util::codec::{Decoder, Encoder, Framed};

pub(crate) struct Peer {
    #[allow(dead_code)]
    addr: SocketAddrV4,
    stream: Framed<TcpStream, MessageFramer>,
    bitfield: Bitfield,
    choked: bool,
}

impl Peer {
    pub async fn new(peer_addr: SocketAddrV4, info_hash: [u8; 20]) -> anyhow::Result<Self> {
        let mut peer = tokio::net::TcpStream::connect(peer_addr)
            .await
            .context("connect to peer")?;
        let mut handshake = Handshake::new(info_hash, *b"00112233445566778899");
        {
            let handshake_bytes = handshake.as_bytes_mut();
            peer.write_all(handshake_bytes)
                .await
                .context("write handshake")?;
            peer.read_exact(handshake_bytes)
                .await
                .context("read handshake")?;
        }
        anyhow::ensure!(handshake.length == 19);
        anyhow::ensure!(&handshake.bittorrent == b"BitTorrent protocol");
        let mut peer = tokio_util::codec::Framed::new(peer, MessageFramer);
        let bitfield = peer
            // method from future_util streamExt
            .next()
            .await
            .expect("peer always sends a bitfields")
            .context("peer message was invalid")?;
        anyhow::ensure!(bitfield.tag == MessageTag::Bitfield);

        Ok(Self {
            addr: peer_addr,
            stream: peer,
            bitfield: Bitfield::from_payload(bitfield.payload),
            choked: true,
        })
    }

    pub(crate) fn has_piece(&self, piece_i: usize) -> bool {
        self.bitfield.has_piece(piece_i)
    }

    pub(crate) async fn participate(
        &mut self,
        piece_i: usize,
        piece_size: usize,
        nblocks: usize,
        submit: kanal::AsyncSender<usize>,
        tasks: kanal::AsyncReceiver<usize>,
        finish: tokio::sync::mpsc::Sender<Message>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.bitfield.has_piece(piece_i));

        self.stream
            .send(Message {
                tag: MessageTag::Interested,
                payload: Vec::new(),
            })
            .await
            .context("send interested message")?;

        // TODO: timeout, error, and return block to submit if .next() timed out
        'task: loop {
            while self.choked {
                let unchoke = self
                    .stream
                    .next()
                    .await
                    .expect("peer always sends an unchoke")
                    .context("peer message was invalid")?;
                match unchoke.tag {
                    MessageTag::Unchoke => {
                        self.choked = false;
                        assert!(unchoke.payload.is_empty());
                        break;
                    }
                    MessageTag::Have => {
                        // TODO: update bitfield
                        // TODO: add to list of peers for relevant piece
                    }
                    MessageTag::Interested
                    | MessageTag::NotInterested
                    | MessageTag::Request
                    | MessageTag::Cancel => {
                        // not allowing requests for now
                    }
                    MessageTag::Piece => {
                        // piece that we no longer need/are responsible for
                    }
                    MessageTag::Choke => {
                        anyhow::bail!("peer sent unchoke while unchoked");
                    }
                    MessageTag::Bitfield => {
                        anyhow::bail!("peer sent bitfield after handshake has been completed");
                    }
                }
            }
            let Ok(block) = tasks.recv().await else {
                break;
            };

            let block_size = if block == nblocks - 1 {
                let md = piece_size % BLOCK_MAX;
                if md == 0 {
                    BLOCK_MAX
                } else {
                    md
                }
            } else {
                BLOCK_MAX
            };

            let mut request = Request::new(
                piece_i as u32,
                (block * BLOCK_MAX) as u32,
                block_size as u32,
            );
            let request_bytes = Vec::from(request.as_bytes_mut());
            self.stream
                .send(Message {
                    tag: MessageTag::Request,
                    payload: request_bytes,
                })
                .await
                .with_context(|| format!("send request for block {block}"))?;

            let mut msg;
            loop {
                msg = self
                    .stream
                    .next()
                    .await
                    .expect("peer always sends a piece")
                    .context("peer message was invalid")?;

                match msg.tag {
                    MessageTag::Choke => {
                        assert!(msg.payload.is_empty());
                        self.choked = true;
                        submit.send(block).await.expect("we still have a receiver");
                        continue 'task;
                    }
                    MessageTag::Piece => {
                        let piece = Piece::ref_from_bytes(&msg.payload[..])
                            .expect("always get all Piece response fields from peer");

                        if piece.index() as usize != piece_i
                            || piece.begin() as usize != block * BLOCK_MAX
                        {
                            // piece that we no longer need/are responsible for
                        } else {
                            assert_eq!(piece.block().len(), block_size);
                            break;
                        }
                    }
                    MessageTag::Have => {
                        // TODO: update bitfield
                        // TODO: add to list of peers for relevant piece
                    }
                    MessageTag::Interested
                    | MessageTag::NotInterested
                    | MessageTag::Request
                    | MessageTag::Cancel => {
                        // not allowing requests for now
                    }
                    MessageTag::Unchoke => {
                        anyhow::bail!("peer sent unchoke while unchoked");
                    }
                    MessageTag::Bitfield => {
                        anyhow::bail!("peer sent bitfield after handshake has been completed");
                    }
                }
            }

            finish.send(msg).await.expect("receiver should not go away while there are active peers (us) and missing blocks (this one)");
        }

        Ok(())
    }
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

#[repr(C)]
pub struct Piece<T: ?Sized = [u8]> {
    index: [u8; 4],
    begin: [u8; 4],
    block: T,
}

impl Piece {
    pub fn index(&self) -> u32 {
        u32::from_be_bytes(self.index)
    }

    pub fn begin(&self) -> u32 {
        u32::from_be_bytes(self.begin)
    }

    pub fn block(&self) -> &[u8] {
        &self.block
    }

    const PIECE_LEAD: usize = mem::size_of::<Piece<()>>();

    pub fn ref_from_bytes(data: &[u8]) -> Option<&Self> {
        if data.len() < Self::PIECE_LEAD {
            return None;
        }
        // NOTE: The slicing here looks really weird. The reason we do it is because we need the
        // length part of the fat pointer to Piece to hold the length of _just_ the `block` field.
        // And the only way we can change the length of the fat pointer to Piece is by changing the
        // length of the fat pointer to the slice, which we do by slicing it. We can't slice it at
        // the front (as it would invalidate the ptr part of the fat pointer), so we slice it at
        // the back!
        let n = data.len();
        // Safety: Piece is a POD with repr(c) and repr(packed), _and_ the fat pointer data length
        // is the length of the trailing DST field (thanks to the PIECE_LEAD offset).
        let piece = &data[..n - Self::PIECE_LEAD] as *const [u8] as *const Piece;
        Some(unsafe { &*piece })
    }
}
#[repr(C, packed)]
pub struct Handshake {
    pub length: u8,
    pub bittorrent: [u8; 19],
    pub resverd: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

impl Handshake {
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20]) -> Handshake {
        Handshake {
            length: 19,
            bittorrent: *b"BitTorrent protocol",
            resverd: [0; 8],
            info_hash,
            peer_id,
        }
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let bytes = self as *mut Self as *mut [u8; std::mem::size_of::<Self>()];
        // Safety: Self is a POD with repr(c) and repr(packed)
        let bytes: &mut [u8; std::mem::size_of::<Self>()] = unsafe { &mut *bytes };
        bytes
    }
}

#[derive(Debug, Clone)]
pub struct Message {
    pub tag: MessageTag,
    pub payload: Vec<u8>,
}

pub struct MessageFramer;

const MAX: usize = 2 << 16;

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
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "message too long",
            ));
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
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid message tag: {}", tag),
                ));
            }
        };

        let data = if src.len() > 5 {
            src[5..4 + length].to_vec()
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
        // Check if the bit at `bit_i` is set.
        byte & 1u8.rotate_right(bit_i + 1) != 0
    }

    #[allow(dead_code)]
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
#[repr(C)]
#[repr(packed)]
pub struct Request {
    index: [u8; 4],
    begin: [u8; 4],
    length: [u8; 4],
}

impl Request {
    pub fn new(index: u32, begin: u32, length: u32) -> Self {
        Self {
            index: index.to_be_bytes(),
            begin: begin.to_be_bytes(),
            length: length.to_be_bytes(),
        }
    }

    pub fn index(&self) -> u32 {
        u32::from_be_bytes(self.index)
    }

    pub fn begin(&self) -> u32 {
        u32::from_be_bytes(self.begin)
    }

    pub fn length(&self) -> u32 {
        u32::from_be_bytes(self.length)
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let bytes = self as *mut Self as *mut [u8; std::mem::size_of::<Self>()];
        // Safety: Self is a POD with repr(c) and repr(packed)
        let bytes: &mut [u8; std::mem::size_of::<Self>()] = unsafe { &mut *bytes };
        bytes
    }
}
