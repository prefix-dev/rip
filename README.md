<a href="https://github.com/prefix-dev/pixi/">
    <picture>
      <source srcset="https://github.com/prefix-dev/rattler_installs_packages/assets/4995967/98a131d5-6a4a-44f1-a60e-dbdcea4951eb" type="image/webp">
      <source srcset="https://github.com/prefix-dev/rattler_installs_packages/assets/4995967/29d2f52b-7801-4a3d-bcc8-2cf34a4263e4" type="image/png">
      <img src="https://github.com/prefix-dev/rattler_installs_packages/assets/4995967/29d2f52b-7801-4a3d-bcc8-2cf34a4263e4" alt="banner">
    </picture>
</a>

# RIP: Fast, barebones **pip** implementation in Rust

![License][license-badge]
[![Build Status][build-badge]][build]
[![Project Chat][chat-badge]][chat-url]
[![docs main][docs-main-badge]][docs-main]

[//]: # ([![crates.io][crates-badge]][crates])

[license-badge]: https://img.shields.io/badge/license-BSD--3--Clause-blue?style=flat-square
[build-badge]: https://img.shields.io/github/actions/workflow/status/prefix-dev/rattler_installs_packages/rust-compile.yml?style=flat-square&branch=main
[build]: https://github.com/prefix-dev/rattler_installs_packages/actions
[chat-badge]: https://img.shields.io/discord/1082332781146800168.svg?label=&logo=discord&logoColor=ffffff&color=7389D8&labelColor=6A7EC2&style=flat-square
[chat-url]: https://discord.gg/kKV8ZxyzY4
[docs-main-badge]: https://img.shields.io/badge/docs-main-yellow.svg?style=flat-square
[docs-main]: https://prefix-dev.github.io/rattler_installs_packages
[crates]: https://crates.io/crates/rattler_installs_packages
[crates-badge]: https://img.shields.io/crates/v/rattler_installs_packages.svg


`RIP` is a library that allows the resolving and installing of Python [PyPi](https://pypi.org/) packages from Rust into a virtual environment. 
It's based on our experience with building [Rattler](https://github.com/mamba-org/rattler) and aims to provide the same
experience but for PyPi instead of Conda.
It should be fast and easy to use. Like Rattler, this library is not a package manager itself but provides the low-level plumbing to be used in one.

`RIP` is based on the quite excellent work of [posy](https://github.com/njsmith/posy) and we have tried to credit
the authors where possible.

# Showcase

Let's resolve the `flask` python package.
We've added a small binary to showcase this:




This showcases the downloading and caching of metadata from PyPi. As well as the package resolution using our solver, more on this below.
We cache everything in a local directory, so that we can re-use the metadata and don't have to download it again.

## Features

This is a list of current and planned features of `RIP`, the biggest are listed below:

* [x] Downloading and aggressive caching of PyPi metadata.
* [x] Resolving of PyPi packages using [Resolvo](https://github.com/mamba-org/resolvo).
* [ ] Installation of wheel files (planned)
* [ ] Support sdist files (planned)

More intricacies of the PyPi ecosystem need to be implemented, see our GitHub issues for more details.


# Solver

We have integrated the stand-alone packaging SAT solver [Resolvo](https://github.com/mamba-org/resolvo), to resolve pypi packages.
This solver is incremental and adds packaging metadata during resolution of the SAT problem.
This feature can be enabled with the `resolvo-pypi` feature flag.


## Contributing 😍

We would love to have you contribute! 
See the CONTRIBUTION.md for more info. For questions, requests or a casual chat, we are very active on our discord server. 
You can [join our discord server via this link][chat-url].
