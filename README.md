<a href="https://github.com/prefix-dev/pixi/">
    <picture>
      <source srcset="https://github.com/prefix-dev/rip/assets/4995967/aab133a8-b335-4942-bf56-335071c76db2" type="image/webp">
      <source srcset="https://github.com/prefix-dev/rip/assets/4995967/3599ae56-42c5-4f3f-9db7-d844fa9558c9" type="image/png">
      <img src="https://github.com/prefix-dev/rip/assets/4995967/3599ae56-42c5-4f3f-9db7-d844fa9558c9" alt="banner">
    </picture>
</a>

# RIP: Fast, barebones **pip** implementation in Rust

![License][license-badge]
[![Build Status][build-badge]][build]
[![Project Chat][chat-badge]][chat-url]
[![docs main][docs-main-badge]][docs-main]

[//]: # ([![crates.io][crates-badge]][crates])

[license-badge]: https://img.shields.io/badge/license-BSD--3--Clause-blue?style=flat-square
[build-badge]: https://img.shields.io/github/actions/workflow/status/prefix-dev/rip/rust-compile.yml?style=flat-square&branch=main
[build]: https://github.com/prefix-dev/rip/actions
[chat-badge]: https://img.shields.io/discord/1082332781146800168.svg?label=&logo=discord&logoColor=ffffff&color=7389D8&labelColor=6A7EC2&style=flat-square
[chat-url]: https://discord.gg/kKV8ZxyzY4
[docs-main-badge]: https://img.shields.io/badge/docs-main-yellow.svg?style=flat-square
[docs-main]: https://prefix-dev.github.io/rip
[crates]: https://crates.io/crates/rattler_installs_packages
[crates-badge]: https://img.shields.io/crates/v/rattler_installs_packages.svg

# Introduction

`RIP` is a library that allows the resolving and installing of Python [PyPI](https://pypi.org/) packages from Rust into a virtual environment.
It's based on our experience with building [Rattler](https://github.com/mamba-org/rattler) and aims to provide the same
experience but for PyPI instead of Conda.

## What should I use this for?

Like Rattler, `RIP` should be fast and easy to use. This library is not a package manager itself but provides the low-level plumbing to be used in one.
To see an example of this take a look at our package manager: [pixi](https://github.com/prefix-dev/pixi)

`RIP` is based on the quite excellent work of [posy](https://github.com/njsmith/posy) and we have tried to credit
the authors where possible.

# Showcase

`RIP` has a very incomplete pip-like binary that can be used to test package installs.
Let's resolve and install the `flask` python package. Running `cargo r install flask /tmp/flask` we get something like this:

![rip-install](https://github.com/prefix-dev/rip/assets/417374/1d55754f-de3a-474f-8ee8-06f7dd098eea)

This showcases the downloading and caching of metadata from PyPI. As well as the package resolution using our incremental SAT solver: [Resolvo](https://github.com/mamba-org/resolvo), more on this below.
Finally, after resolution it installs the package into a venv.
We cache everything locally so that we can re-use the PyPi metadata.

## Features

This is a list of current features of `RIP`, the biggest are listed below:

* [x] Async downloading and aggressive caching of PyPI metadata.
* [x] Resolving of PyPI packages using [Resolvo](https://github.com/mamba-org/resolvo).
* [x] Installation of wheel files.
* [x] Support sdist files (must currently adhere to the `PEP 517` and `PEP 518` standards).
* [x] Caching of locally built wheels.

More intricacies of the PyPI ecosystem need to be implemented, see our GitHub issues for more details.

# Details

## Resolving

We have integrated the stand-alone packaging SAT solver [Resolvo](https://github.com/mamba-org/resolvo), to resolve pypi packages.
This solver is incremental and adds packaging metadata during resolution of the SAT problem.
This feature can be enabled with the `resolvo` feature flag.

## Installation

We have very simple installation support for the resolved packages.
This should be used for testing purposes exclusively
E.g. `cargo r -- install flask /tmp/flask_env` to create a venv and install the flask and it's into it.
After which you can run:
   1. `/tmp/flask_env/bin/python` to start python in the venv.
   2. `import flask #`, this should import the flask package from the venv.
There is no detection of existing packages in the venv yet, although this should be relatively straightforward.


# Contributing 😍

We would love to have you contribute!
See the [CONTRIBUTING.md](./CONTRIBUTING.md) for more info. For questions, requests or a casual chat, we are very active on our discord server.
You can [join our discord server via this link][chat-url].
