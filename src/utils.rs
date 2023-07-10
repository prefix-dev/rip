/// Keep retrying a certain IO function until it either succeeds or until it doesnt return
/// [`std::io::ErrorKind::Interrupted`].
pub fn retry_interrupted<F, T>(mut f: F) -> std::io::Result<T>
where
    F: FnMut() -> std::io::Result<T>,
{
    loop {
        match f() {
            Ok(result) => return Ok(result),
            Err(err) if err.kind() != std::io::ErrorKind::Interrupted => {
                return Err(err);
            }
            _ => {
                // Otherwise keep looping!
            }
        }
    }
}
