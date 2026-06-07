pub use crate::worker::{GenericRunner, Worker};

mod worker;

pub trait Service {
    fn serve(&self);
}

pub struct Client;

impl Client {
    pub fn new() -> Self {
        Self
    }

    pub fn drive(&self, worker: Worker) {
        worker.serve();
        GenericRunner::<Worker>::run(worker);
    }
}

fn generic_call<T: Service>(value: &T) {
    value.serve();
}

pub fn entry() {
    let worker = Worker::new();
    Client::new().drive(worker);
    generated_call!(Worker::new());
}
