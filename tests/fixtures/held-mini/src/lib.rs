/// Opens the local database.
#[uniffi::export]
pub fn open_database() {}

pub struct DatabaseHandle {
    id: String,
}
