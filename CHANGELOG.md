# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).


## [0.4.0] - 2024-01-18
### ✨ Highlights
- Venv creation, used for wheel building, should work correctly on windows now

### 📃 Details

#### Added
- Add missing files for windows when creating venv

#### Changed
- Use python location for venv in bin

## [0.3.0] - 2024-01-16
### ✨ Highlights

Added some small functionality to `rip_bin`:
    - Can now use `-p, --python-interpreter` to use a non-system version of python when resolving.
    - Wheel building process inherites environment variables use `-c, --clean-env` when running the binary to not use this.

### 📃 Details

#### Added
- Add ability to specify python interpreter option to the rip bin

#### Changed
- Use `fs_err` instead of `std::fs` for better error messages
- Pass environment variables to wheel building

## [0.2.1] - 2024-01-12

### 📃 Details

#### Fixed
- Using too constraining lifetime for WheelBuilder

## [0.2.0] - 2024-01-11
### ✨ Highlights

- Fixed some issues regarding python source dists not building

### 📃 Details

#### Added
- Adds the ability to specify the python interpeter for wheel building
- Add changelogs

#### Changed
- Installation into binary
- Create venv from rust
- Switch the interpreter to the build options

#### Fixed
- Error in archive file matching functionality for sdists
- Modify the PATH when running metadata build or actual build for wheels

## [0.1.0] - 2023-12-08
### ✨ Highlights

- First rip release!

### 📃 Details

#### Added
- Add rip executable
- Added functions to analyze the extras field
- Add ci, release info and workspace
- Add README
- Added LICENSE file
- Add pre-commit config
- Add locked and favored packages to the solver
- Add Borrow impl for Extra and NormalizedPackageName
- Add support for entry points when installing wheels
- Support script files and #!python rewriting
- Add derive debug

#### Changed
- Skip errornous artifacts
- Update to main branch
- Enable use http API instead of json
- Move to new rattler version
- Now does things lazily
- Solves lazily
- Formatting fixes
- Indexing program to query pypi stuff
- Extras are working
- Update rattler
- Update rattler
- Use latest vesion of solver
- Use rusttls feature
- Update banner
- Don't cache metadata cause we are storing it already
- Extract and check wheel tags
- Use pep440_rs and pep508_rs
- Unpacking wheels
- Find installed distributions
- Uninstall a python distribution from an environment
- Use WheelTag in Distribution
- Detect also python3 executable if available
- Changed env marker logging to debug
- Range requests for wheel
- Read build-info from pyproject.toml
- Get the system python interpreter version
- Create virtual environment
- Spooled local file and cleanup
- Refactored package-db module to make it a bit less generic
- Refactored name parsing from artifacts
- Expose dist-info folder after install
- Metadata extraction and wheel building for sdist packages
- Move build env
- Implement bytecode/pyc compilation
- Headers data category
- Wheel cache for built sdists
- Sdists can now build sdists if needed
- We need wheel-builder to be public

#### Fixed
- Build is working again
- Fixed tests
- Clippy and resolvo
- Clean up docs a little bit
- Index to seperate crate and ci issues
- Formatting
- Doc hyperlink
- Rustls-tls feature
- Banner image
- Gif in readme
- Docs link
- Links in badges
- Fix performance around extras
- Ignore invalid requirements
- Feature in readme
- Allow wildcards in ambiguos specifiers
- CONTRIBUTING, Cargo.toml and README.md
- Expose InstallOptions renamed as UnpackWheelOptions
- Fix if a package name has multiple dashes
- Now check metadata version
- Wheel tags can contain compound tags
- Fixes actual wheel building
- Ambigious http import
- Move empty artifact error up
- More reliable byte code compiler

#### Removed
- Remove empty folders on uninstall
