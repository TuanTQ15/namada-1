#[cfg(not(feature = "migrations"))]
use core::fmt::Formatter;
#[cfg(feature = "migrations")]
use core::fmt::{Display, Formatter};
#[cfg(feature = "migrations")]
use core::str::FromStr;

use borsh::{BorshDeserialize, BorshSerialize};
use borsh_ext::BorshSerializeExt;
use data_encoding::HEXUPPER;
use namada_core::storage::Key;
#[cfg(feature = "migrations")]
use namada_migrations::get_deserializer;
#[cfg(feature = "migrations")]
use namada_migrations::TypeHash;
use regex::Regex;
use serde::de::{Error, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub trait DBUpdateVisitor {
    fn read(&self, key: &Key) -> Option<Vec<u8>>;
    fn write(&mut self, key: &Key, value: impl AsRef<[u8]>);
    fn delete(&mut self, key: &Key);
    fn get_pattern(&self, pattern: Regex) -> Vec<(String, Vec<u8>)>;
}

#[derive(Clone, Debug, BorshSerialize, BorshDeserialize)]
/// A value to be added to the database that can be
/// validated.
pub struct UpdateValue {
    type_hash: [u8; 32],
    bytes: Vec<u8>,
}

#[cfg(feature = "migrations")]
impl<T: TypeHash + BorshSerialize> From<T> for UpdateValue {
    fn from(value: T) -> Self {
        Self {
            type_hash: T::HASH,
            bytes: value.serialize_to_vec(),
        }
    }
}

struct UpdateValueVisitor;

impl<'de> Visitor<'de> for UpdateValueVisitor {
    type Value = UpdateValue;

    fn expecting(&self, formatter: &mut Formatter) -> core::fmt::Result {
        formatter.write_str(
            "a hex encoded series of bytes that borsh decode to an \
             UpdateValue.",
        )
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: Error,
    {
        UpdateValue::try_from_slice(
            &HEXUPPER
                .decode(v.as_bytes())
                .map_err(|e| E::custom(e.to_string()))?,
        )
        .map_err(|e| E::custom(e.to_string()))
    }
}

impl Serialize for UpdateValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let hex_bytes = HEXUPPER.encode(&self.serialize_to_vec());
        Serialize::serialize(&hex_bytes, serializer)
    }
}

impl<'de> Deserialize<'de> for UpdateValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UpdateValueVisitor)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// An update to the database
pub enum DbUpdateType {
    Add {
        key: Key,
        value: UpdateValue,
        force: bool,
    },
    Delete(Key),
    RepeatAdd {
        pattern: String,
        value: UpdateValue,
        force: bool,
    },
    RepeatDelete(String),
}

#[cfg(feature = "migrations")]
impl DbUpdateType {
    /// Get the key being modified
    pub fn key(&self) -> String {
        match self {
            DbUpdateType::Add { key, .. } => key.to_string(),
            DbUpdateType::Delete(key) => key.to_string(),
            DbUpdateType::RepeatAdd { pattern, .. } => pattern.to_string(),
            DbUpdateType::RepeatDelete(pattern) => pattern.to_string(),
        }
    }

    /// Validate that the contained value deserializes correctly given its data
    /// hash.
    pub fn validate(&self) -> eyre::Result<()> {
        match self {
            DbUpdateType::RepeatAdd { value, .. }
            | DbUpdateType::Add { value, .. } => {
                let deserializer =
                    namada_migrations::get_deserializer(&value.type_hash)
                        .ok_or_else(|| {
                            eyre::eyre!(
                                "Type hash {:?} did not correspond to a \
                                 deserializer in TYPE_DESERIALIZERS.",
                                value.type_hash
                            )
                        })?;
                _ = deserializer(value.bytes.clone()).ok_or_else(|| {
                    eyre::eyre!(
                        "The value {:?} could not be successfully deserialized",
                        value
                    )
                })?;
                Ok(())
            }
            DbUpdateType::Delete(_) | DbUpdateType::RepeatDelete(_) => Ok(()),
        }
    }

    /// Validate a DB change and persist it if so. The debug representation of
    /// the new value is returned for logging purposes.
    #[allow(dead_code)]
    pub fn update<DB: DBUpdateVisitor>(
        &self,
        db: &mut DB,
    ) -> eyre::Result<UpdateStatus> {
        match self {
            Self::Add { key, value, force } => {
                let deserialized = if !force {
                    let deserializer =
                        namada_migrations::get_deserializer(&value.type_hash)
                            .ok_or_else(|| {
                            eyre::eyre!(
                                "Type hash {:?} did not correspond to a \
                                 deserializer in TYPE_DESERIALIZERS.",
                                value.type_hash
                            )
                        })?;
                    let deserialized = deserializer(value.bytes.clone())
                        .ok_or_else(|| {
                            eyre::eyre!(
                                "The value {:?} for key {} could not be \
                                 successfully deserialized",
                                value,
                                key
                            )
                        })?;
                    if let Some(prev) = db.read(key) {
                        deserializer(prev).ok_or_else(|| {
                            eyre::eyre!(
                                "The previous value under the key {} did not \
                                 have the same type as that provided: Input \
                                 was {}",
                                key,
                                deserialized
                            )
                        })?;
                    }
                    Some(deserialized)
                } else {
                    None
                };
                db.write(key, &value.bytes);
                Ok(deserialized
                    .map(|d| UpdateStatus::Add(vec![(key.to_string(), d)]))
                    .unwrap_or_else(|| UpdateStatus::Add(vec![])))
            }
            Self::Delete(key) => {
                db.delete(key);
                Ok(UpdateStatus::Deleted(vec![key.to_string()]))
            }
            DbUpdateType::RepeatAdd {
                pattern,
                value,
                force,
            } => {
                let pattern = Regex::new(pattern).unwrap();
                let mut pairs = vec![];
                let (deserialized, deserializer) = if !force {
                    let deserializer =
                        namada_migrations::get_deserializer(&value.type_hash)
                            .ok_or_else(|| {
                            eyre::eyre!(
                                "Type hash {:?} did not correspond to a \
                                 deserializer in TYPE_DESERIALIZERS.",
                                value.type_hash
                            )
                        })?;
                    let deserialized = deserializer(value.bytes.clone())
                        .ok_or_else(|| {
                            eyre::eyre!(
                                "The value {:?} for pattern {} could not be \
                                 successfully deserialized",
                                value,
                                pattern,
                            )
                        })?;
                    (Some(deserialized), Some(deserializer))
                } else {
                    (None, None)
                };
                for (key, prev) in db.get_pattern(pattern.clone()) {
                    if let (Some(func), Some(d)) =
                        (deserializer, deserialized.as_ref())
                    {
                        func(prev).ok_or_else(|| {
                            eyre::eyre!(
                                "The previous value under the key {} did not \
                                 have the same type as that provided: Input \
                                 was {}",
                                key,
                                d,
                            )
                        })?;
                        pairs.push((key.to_string(), d.clone()));
                    }
                    db.write(&Key::from_str(&key).unwrap(), &value.bytes);
                }
                Ok(UpdateStatus::Add(pairs))
            }
            DbUpdateType::RepeatDelete(pattern) => {
                let pattern = Regex::new(pattern).unwrap();
                Ok(UpdateStatus::Deleted(
                    db.get_pattern(pattern.clone())
                        .into_iter()
                        .map(|(key, _)| {
                            db.delete(&Key::from_str(&key).unwrap());
                            key
                        })
                        .collect(),
                ))
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct DbChanges {
    pub changes: Vec<DbUpdateType>,
}

#[cfg(feature = "migrations")]
impl Display for DbUpdateType {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            DbUpdateType::Add {
                key,
                value: UpdateValue { type_hash, bytes },
                ..
            } => {
                let Some(deserializer) = get_deserializer(type_hash) else {
                    return f.write_str(&format!(
                        "Type hash {:?} did not correspond to a deserializer \
                         in TYPE_DESERIALIZERS.",
                        type_hash
                    ));
                };

                let Some(value) = deserializer(bytes.clone()) else {
                    return f.write_str(&format!(
                        "The value {:?} for key <{}> could not be \
                         successfully deserialized",
                        bytes, key
                    ));
                };

                f.write_str(&format!(
                    "Write to key: <{}> with value: {}",
                    key, value
                ))
            }
            DbUpdateType::Delete(key) => {
                f.write_str(&format!("Delete key: <{}>", key))
            }
            DbUpdateType::RepeatAdd {
                pattern,
                value: UpdateValue { type_hash, bytes },
                ..
            } => {
                let Some(deserializer) = get_deserializer(type_hash) else {
                    return f.write_str(&format!(
                        "Type hash {:?} did not correspond to a deserializer \
                         in TYPE_DESERIALIZERS.",
                        type_hash
                    ));
                };

                let Some(value) = deserializer(bytes.clone()) else {
                    return f.write_str(&format!(
                        "The value {:?} for pattern <{}> could not be \
                         successfully deserialized",
                        bytes, pattern
                    ));
                };

                f.write_str(&format!(
                    "Write to pattern: <{}> with value: {}",
                    pattern, value
                ))
            }
            DbUpdateType::RepeatDelete(pattern) => {
                f.write_str(&format!("Delete pattern: <{}>", pattern))
            }
        }
    }
}

pub enum UpdateStatus {
    Deleted(Vec<String>),
    Add(Vec<(String, String)>),
}

#[cfg(feature = "migrations")]
impl Display for UpdateStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Deleted(keys) => {
                for key in keys {
                    f.write_str(&format!("Deleting key <{}>", key))?;
                }
            }
            Self::Add(pairs) => {
                for (k, v) in pairs {
                    f.write_str(&format!(
                        "Writing key <{}> with value: {}",
                        k, v
                    ))?;
                }
            }
        }
        Ok(())
    }
}
