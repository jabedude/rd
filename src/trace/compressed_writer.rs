#[derive(Copy, Clone, Eq, PartialEq)]
pub enum Sync {
    DontSync,
    Sync,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum WaitFlag {
    Wait,
    NoWait,
}

pub struct BlockHeader {
    pub compressed_length: u32,
    pub uncompressed_length: u32,
}

/// CompressedWriter opens an output file and writes compressed blocks to it.
/// Blocks of a fixed but unspecified size (currently 1MB) are compressed.
/// Each block of compressed data is written to the file preceded by two
/// 32-bit words: the size of the compressed data (excluding block header)
/// and the size of the uncompressed data, in that order. See BlockHeader below.
///
/// We use multiple threads to perform compression. The threads are
/// responsible for the actual data writes. The thread that creates the
/// CompressedWriter is the "producer" thread and must also be the caller of
/// 'write'. The producer thread may block in 'write' if 'buffer_size' bytes are
/// being compressed.
///
/// Each data block is compressed independently using zlib.
pub struct CompressedWriter;