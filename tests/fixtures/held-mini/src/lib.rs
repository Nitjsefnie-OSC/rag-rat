/// Opens the local database.
#[uniffi::export]
pub fn open_database() {}

pub const MAX_OPEN_DATABASES: usize = 4;
pub static DEFAULT_DATABASE_NAME: &str = "held";
pub type DatabaseId = String;

macro_rules! database_event {
    ($name:expr) => {
        $name
    };
}

pub mod handles {
    pub fn close_database() {}
}

pub struct DatabaseHandle {
    id: String,
}

impl DatabaseHandle {
    pub fn id(&self) -> &str {
        &self.id
    }
}

pub enum DatabaseState {
    Open,
    Closed,
}

pub trait DatabaseLifecycle {
    fn open(&self);
}
