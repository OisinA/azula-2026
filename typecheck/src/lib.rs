#[macro_use]
extern crate maplit;

mod generics;
mod typecheck;

pub mod prelude {
    pub use crate::typecheck::Typechecker;
}
