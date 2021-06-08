#![deny(missing_docs)]
//! Dummy crate needs high-level documentation.
/// Dummy public function needs documentation.
pub fn it_works() {
    assert!(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_it_works() {
        it_works()
    }
}
