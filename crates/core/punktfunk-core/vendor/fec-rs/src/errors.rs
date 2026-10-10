use core::fmt;

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Error {
    TooFewShards,
    TooManyShards,
    TooFewDataShards,
    TooManyDataShards,
    TooFewParityShards,
    TooManyParityShards,
    TooFewBufferShards,
    TooManyBufferShards,
    IncorrectShardSize,
    TooFewShardsPresent,
    EmptyShard,
    InvalidIndex,
    InvalidParityMatrix,
    SingularMatrix,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::TooFewShards => write!(f, "Too few shards"),
            Error::TooManyShards => write!(f, "Too many shards"),
            Error::TooFewDataShards => write!(f, "Too few data shards"),
            Error::TooManyDataShards => write!(f, "Too many data shards"),
            Error::TooFewParityShards => write!(f, "Too few parity shards"),
            Error::TooManyParityShards => write!(f, "Too many parity shards"),
            Error::TooFewBufferShards => write!(f, "Too few buffer shards"),
            Error::TooManyBufferShards => write!(f, "Too many buffer shards"),
            Error::IncorrectShardSize => write!(f, "Incorrect shard size"),
            Error::TooFewShardsPresent => write!(f, "Too few shards present for reconstruction"),
            Error::EmptyShard => write!(f, "Empty shard"),
            Error::InvalidIndex => write!(f, "Invalid index"),
            Error::InvalidParityMatrix => write!(f, "Invalid parity matrix"),
            Error::SingularMatrix => write!(f, "Singular matrix"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum SBSError {
    TooManyCalls,
    LeftoverShards,
    RSError(Error),
}

impl fmt::Display for SBSError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SBSError::TooManyCalls => write!(f, "Too many calls"),
            SBSError::LeftoverShards => write!(f, "Leftover shards"),
            SBSError::RSError(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SBSError {}
