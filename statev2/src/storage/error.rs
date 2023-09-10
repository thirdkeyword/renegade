//! Error types for storage access

use std::{error::Error, fmt::Display};

use flexbuffers::{
    DeserializationError as FlexbuffersDeserializationError,
    SerializationError as FlexbuffersSerializationError,
};
use libmdbx::Error as MdbxError;

#[derive(Debug)]
pub enum StorageError {
    /// Error creating a new transaction in the database
    BeginTx(MdbxError),
    /// Error committing a transaction
    Commit(MdbxError),
    /// Error deserializing a value from storage
    Deserialization(FlexbuffersDeserializationError),
    /// Failure opening the database
    OpenDb(MdbxError),
    /// Failure opening a table in the database
    OpenTable(MdbxError),
    /// Error serializing a value for storage
    Serialization(FlexbuffersSerializationError),
    /// Error while performing a transaction operation
    TxOp(MdbxError),
}

impl Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StorageError: {self:?}")
    }
}

impl Error for StorageError {}
