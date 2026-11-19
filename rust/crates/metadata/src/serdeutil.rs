
use serde::{Deserialize, Deserializer};

/// Go's null-into-zero-value behaviour: `null` becomes `T::default()`, any
/// other value deserializes as `T`.
pub(crate) fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
