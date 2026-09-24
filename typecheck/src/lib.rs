#[macro_use]
extern crate maplit;

mod closures;
mod generics;
mod typecheck;

pub mod prelude {
    pub use crate::typecheck::Typechecker;
}
