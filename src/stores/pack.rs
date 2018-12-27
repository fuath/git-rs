use flate2::bufread::DeflateDecoder;
use std::io::{BufReader, SeekFrom};
use std::path::{Path, PathBuf};
use std::io::prelude::*;
use std::fs::File;
use std;

use crate::delta::{DeltaDecoder, OFS_DELTA, REF_DELTA};
use crate::errors::{Result, ErrorKind};
use crate::packindex::Index;
use crate::objects::Type;
use crate::id::Id;

type GetObject = Fn(&Id) -> Result<Option<(Type, Box<std::io::Read>)>>;

pub struct Store<R> {
    read: Box<Fn() -> Result<R>>,
    index: Index
}

// pack format is:
//
//      4 byte magic number ('P', 'A', 'C', 'K')
//      4 byte version number (2 or 3)
//      4 byte object count (N)
//      N objects
//      20 byte checksum

fn build_index(reader: Box<std::io::Read>) -> Result<Index> {
    Err(ErrorKind::NotImplemented.into())
}

impl<R: std::io::Read + std::io::Seek + 'static> Store<R> {
    pub fn new<C>(func: C, index: Option<Index>) -> Result<Self>
        where C: Fn() -> Result<R> + 'static {

        let idx = match index {
            Some(xs) => xs,
            None => build_index(Box::new(func()?))?
        };

        Ok(Store {
            read: Box::new(func),
            index: idx
        })
    }

    pub fn read_bounds (&self, start: u64, end: u64, get_object: &GetObject) -> Result<(u8, Box<std::io::Read>)> {
        let handle = (self.read)()?;
        let mut buffered_file = BufReader::new(handle);
        buffered_file.seek(SeekFrom::Start(start))?;
        let stream = buffered_file.take(end - start);

        // type + size bytes
        let mut continuation = 0;
        let mut type_flag = 0;
        let mut size_vec = Vec::new();
        let mut byte = [0u8; 1];

        let mut take_one = stream.take(1);
        take_one.read_exact(&mut byte)?;
        let mut original_stream = take_one.into_inner();
        continuation = byte[0] & 0x80;
        type_flag = (byte[0] & 0x70) >> 4;
        size_vec.push(byte[0] & 0x0f);
        loop {
            if continuation < 1 {
                break
            }

            take_one = original_stream.take(1);
            take_one.read_exact(&mut byte)?;
            original_stream = take_one.into_inner();
            continuation = byte[0] & 0x80;
            size_vec.push(byte[0] & 0x7f); 
        }
        let mut object_stream = original_stream;

        let count = size_vec.len();
        let mut size = match size_vec.pop() {
            Some(xs) => xs as u64,
            None => return Err(ErrorKind::CorruptedPackfile.into())
        };
        while size_vec.len() > 0 {
            let next = match size_vec.pop() {
                Some(xs) => xs as u64,
                None => return Err(ErrorKind::CorruptedPackfile.into())
            };
            size |= next << (4 + 7 * (count - size_vec.len()));
        }

        match type_flag {
            0...4 => {
                let mut zlib_header = [0u8; 2];
                object_stream.read_exact(&mut zlib_header)?;
                Ok((type_flag, Box::new(DeflateDecoder::new(object_stream))))
            },

            OFS_DELTA => {
                let mut take_one = object_stream.take(1);
                take_one.read_exact(&mut byte)?;
                let mut offset = (byte[0] & 0x7F) as u64;
                let mut original_stream = take_one.into_inner();

                while byte[0] & 0x80 > 0 {
                    offset += 1;
                    offset <<= 7;
                    take_one = original_stream.take(1);
                    take_one.read_exact(&mut byte)?;
                    offset += (byte[0] & 0x7F) as u64;
                    original_stream = take_one.into_inner();
                }

                let mut zlib_header = [0u8; 2];
                original_stream.read_exact(&mut zlib_header)?;
                let mut deflate_stream = DeflateDecoder::new(original_stream);
                let mut instructions = Vec::new();
                deflate_stream.read_to_end(&mut instructions);

                let (base_type, stream) = match self.read_bounds(start - offset, start, get_object) {
                    Ok(xs) => xs,
                    Err(e) => return Err(e)
                };

                Ok((base_type, Box::new(DeltaDecoder::new(&instructions, stream))))
            },

            REF_DELTA => {
                let mut ref_bytes = [0u8; 20];
                object_stream.read_exact(&mut ref_bytes)?;
                let id = Id::from(&ref_bytes);

                let mut zlib_header = [0u8; 2];
                object_stream.read_exact(&mut zlib_header)?;
                let mut deflate_stream = DeflateDecoder::new(object_stream);
                let mut instructions = Vec::new();
                deflate_stream.read_to_end(&mut instructions);

                let (t, base_stream) = match get_object(&id)? {
                    Some((xs, stream)) => match xs {
                        Type::Commit => (1, stream),
                        Type::Tree => (2, stream),
                        Type::Blob => (3, stream),
                        Type::Tag => (4, stream)
                    },
                    None => return Err(ErrorKind::CorruptedPackfile.into())
                };

                Ok((t, Box::new(DeltaDecoder::new(&instructions, base_stream))))
            },

            _ => {
                return Err(ErrorKind::BadLooseObject.into())
            }
        }
    }

    fn get(&self, id: &Id, get_object: &GetObject) -> Result<Option<(Type, Box<std::io::Read>)>> {
        let (start, end) = match self.index.get_bounds(&id) {
            Some(xs) => xs,
            None => return Ok(None)
        };
        let (t, stream) = self.read_bounds(start, end, get_object)?;
        let typed = match t {
            1 => Type::Commit,
            2 => Type::Tree,
            3 => Type::Blob,
            4 => Type::Tag,
            _ => return Err(ErrorKind::CorruptedPackfile.into())
        };

        Ok(Some((typed, stream)))
    }
}

#[cfg(test)]
mod tests {

    use super::Index;
    use super::Store;
    use super::Id;
    use std::io::Cursor;

    #[test]
    fn can_load() {
        let bytes = include_bytes!("../../fixtures/pack_index");

        let idx = Index::from(&mut bytes.as_ref()).expect("bad index");
        let pack = Store::new(|| Ok(Cursor::new(include_bytes!("../../fixtures/packfile") as &[u8])), Some(idx)).expect("bad packfile");

        let id = Id::from_str("872e26b3fbebe64a2a85b271fed6916b964b4fde").unwrap();
        let (kind, stream) = pack.get(&id, &|_| Ok(None)).expect("failure").unwrap();

    }
}