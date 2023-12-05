use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::{
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{channel, Sender},
};
use tempfile::NamedTempFile;
use thiserror::Error;

/// The result of compiling a Python script to byte code.
type CompilationResponse = Result<PathBuf, CompilationError>;
type CompilationRequest = PathBuf;

type BoxedCallback = Box<dyn FnOnce(CompilationResponse) + Send + 'static>;
type CompilationCallbackMap = HashMap<CompilationRequest, Vec<BoxedCallback>>;

/// An error that can occur when compiling a source file.
#[derive(Debug, Error, Clone)]
pub enum CompilationError {
    /// The file is not a python file.
    #[error("not a python file")]
    NotAPythonFile,

    /// The file could not be found.
    #[error("source file not found")]
    SourceNotFound,

    /// The file could not be compiled
    #[error("failed to compile")]
    FailedToCompile,

    /// The compilation host unexpectedly quit.
    #[error("host has quit")]
    HostQuit,
}

/// An error that can occur when spawning the compilation host.
#[derive(Debug, Error)]
pub enum SpawnCompilerError {
    /// Could not create the source code that runs in the compilation host.
    #[error("failed to create temporary file for compilation source")]
    FailedToCreateSource(#[source] io::Error),

    /// Failed to start the python executable
    #[error("failed to start python executable")]
    FailedToStartPython(#[source] io::Error),
}

/// An object that allows compiling python source code to byte code in a separate process.
pub struct ByteCodeCompiler {
    /// The channel that is used to send compilation requests to the compilation host. If this is
    /// dropped the attached thread will drop stdin of the child which will signal the child to
    /// exit.
    request_tx: Option<Sender<CompilationRequest>>,

    /// Callback functions per compilation request. These are called when the compilation host
    /// finishes processing a request.
    pending_callbacks: Arc<Mutex<Option<CompilationCallbackMap>>>,

    /// The child process. This is waited upon on drop.
    child: Option<std::process::Child>,

    // The file that contains the python source code of the compilation host. We keep this around
    // to make sure the file is not accidentally deleted while the compilation host is still using
    // it.
    _compilation_source: NamedTempFile,
}

impl ByteCodeCompiler {
    /// Constructs a new instance.
    ///
    /// This function spawns a new python process that will be used to compile python source code.
    pub fn new(python_path: &Path) -> Result<Self, SpawnCompilerError> {
        // Write the compilation host source code to a temporary file
        let compilation_source = tempfile::Builder::new()
            .prefix("pyc_compilation_host")
            .suffix(".py")
            .tempfile()
            .and_then(|mut f| {
                f.write_all(include_bytes!("compile_pyc.py"))?;
                Ok(f)
            })
            .map_err(SpawnCompilerError::FailedToCreateSource)?;

        // Start the compilation process
        let mut child = Command::new(python_path)
            .arg("-Wi")
            .arg("-u")
            .arg(compilation_source.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(SpawnCompilerError::FailedToStartPython)?;

        // Spawn a thread to process incoming compilation requests and forward them to the input of the
        // compilation process
        let (request_tx, request_rx) = channel::<CompilationRequest>();
        let mut child_stdin = child.stdin.take().expect("stdin is piped");
        std::thread::spawn(move || {
            while let Ok(request) = request_rx.recv() {
                tracing::trace!("compiling {}", request.display());
                if let Err(e) = child_stdin
                    .write_all(request.to_string_lossy().as_bytes())
                    .and_then(|_| child_stdin.write_all(b"\n"))
                    .and_then(|_| child_stdin.flush())
                {
                    tracing::error!("unexpected error writing to compilation host stdin: {}", e);
                    break;
                };
            }
        });

        // Spawn another thread to process the output of the compilation process and forward it to the
        // response channel.
        let pending_callbacks = Arc::new(Mutex::new(Some(CompilationCallbackMap::new())));
        let response_callbacks = pending_callbacks.clone();
        let child_stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
        std::thread::spawn(move || {
            #[derive(Debug, serde::Deserialize)]
            struct Response {
                path: PathBuf,
                output_path: Option<PathBuf>,
            }

            for line in child_stdout.lines() {
                match line.and_then(|line| Ok(serde_json::from_str::<Response>(&line)?)) {
                    Ok(response) => {
                        tracing::trace!("finished compiling '{}'", response.path.display());

                        let callbacks = {
                            let mut callback_lock = response_callbacks.lock();
                            let callbacks = callback_lock.as_mut().expect(
                                "the callbacks are not dropped until the end of this function",
                            );
                            callbacks.remove(&response.path)
                        };
                        match callbacks {
                            None => panic!(
                                "received a response for an unknown request '{}'",
                                response.path.display()
                            ),
                            Some(callbacks) => {
                                for callback in callbacks {
                                    callback(match &response.output_path {
                                        Some(output_path) => Ok(output_path.to_path_buf()),
                                        None => Err(CompilationError::FailedToCompile),
                                    })
                                }
                            }
                        };
                    }
                    Err(err) => {
                        tracing::error!(
                            "unexpected error reading from compilation host stdout: {}",
                            err
                        );
                        break;
                    }
                }
            }

            tracing::trace!("compilation host stdout closed");

            // Abort any pending callbacks and disable the ability to add new ones.
            let callbacks = response_callbacks
                .lock()
                .take()
                .expect("only we can drop the callbacks");
            for (_, callbacks) in callbacks {
                for callback in callbacks {
                    callback(Err(CompilationError::HostQuit))
                }
            }
        });

        Ok(Self {
            request_tx: Some(request_tx),
            pending_callbacks,
            child: Some(child),
            _compilation_source: compilation_source,
        })
    }

    /// Queue the compilation of the specified python file.
    ///
    /// The file is send to the compilation host which will immediately start compiling the file.
    /// The callback is called when the compilation is finished.
    ///
    /// Use the `drain` method to wait for the compilation to finish.
    pub fn compile<F: FnOnce(CompilationResponse) + Send + 'static>(
        &self,
        source_path: &Path,
        callback: F,
    ) -> Result<(), CompilationError> {
        if source_path.extension() != Some(std::ffi::OsStr::new("py")) {
            return Err(CompilationError::NotAPythonFile);
        }

        if !source_path.is_file() {
            return Err(CompilationError::SourceNotFound);
        }

        let mut lock = self.pending_callbacks.lock();
        let Some(callbacks) = lock.as_mut() else {
            return Err(CompilationError::HostQuit);
        };

        callbacks
            .entry(source_path.to_path_buf())
            .or_default()
            .push(Box::new(callback));

        self.request_tx
            .as_ref()
            .expect("the channel is only dropped on drop")
            .send(source_path.to_owned())
            .map_err(|_| CompilationError::HostQuit)
    }

    /// Compile the specified python file and wait for the compilation to finish.
    pub fn compile_and_wait(&self, source_path: &Path) -> Result<PathBuf, CompilationError> {
        let (tx, rx) = channel::<CompilationResponse>();
        self.compile(source_path, move |result| {
            tx.send(result)
                .expect("the reader is waiting on this response");
        })?;
        rx.recv()
            .expect("the sender was dropped before sending a response")
    }

    /// Wait for all queued compilations to finish.
    pub fn wait(mut self) -> Result<(), std::io::Error> {
        // Drop the request channel to signal the compilation host that we are done. This will
        // ensure that the stdin pipe of the compilation host is closed which will signal the host
        // to exit.
        drop(self.request_tx.take());

        // Wait for the compilation host to exit
        self.child
            .take()
            .expect("the child is only dropped on drop")
            .wait()?;

        Ok(())
    }
}

impl Drop for ByteCodeCompiler {
    fn drop(&mut self) {
        drop(self.request_tx.take());
        if let Some(mut child) = self.child.take() {
            child.wait().unwrap();
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::python_env::system_python_executable;

    #[test]
    fn test() {
        let python_path = system_python_executable().unwrap();

        // Create a temporary file that holds the sourcecode of the compilation host
        let mut compiler_source = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        compiler_source
            .write_all(include_bytes!("compile_pyc.py"))
            .unwrap();

        // Spawn a compiler and compile the compilation host source code.
        let compiler = ByteCodeCompiler::new(&python_path).unwrap();
        let pyc_file = compiler.compile_and_wait(compiler_source.path()).unwrap();

        // Make sure the compiled file exists
        assert!(pyc_file.is_file(), "The compiled file does not exist!");

        // Wait should immediately return
        compiler.wait().unwrap();
    }

    #[test]
    fn test_failed_case() {
        let python_path = system_python_executable().unwrap();

        // Create a temporary file that holds the sourcecode of the compilation host
        let mut compiler_source = tempfile::Builder::new().suffix(".py").tempfile().unwrap();
        compiler_source.write_all(b"$").unwrap();

        // Spawn a compiler and compile the compilation host source code.
        let compiler = ByteCodeCompiler::new(&python_path).unwrap();
        compiler
            .compile_and_wait(compiler_source.path())
            .unwrap_err();
    }
}
