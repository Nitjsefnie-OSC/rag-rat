use crate::Service;

pub struct Worker;

impl Worker {
    pub fn new() -> Self {
        Self
    }
}

impl Service for Worker {
    fn serve(&self) {}
}

pub struct GenericRunner<T> {
    _value: std::marker::PhantomData<T>,
}

impl<T: Service> GenericRunner<T> {
    pub fn run(value: T) {
        value.serve();
    }
}
