use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rattler_installs_packages::html::{parse_package_names_html, parse_project_info_html};
use std::str::FromStr;
use url::Url;

fn parse_project_info(c: &mut Criterion) {
    let html = r#"<html>
                <head>
                  <meta name="pypi:repository-version" content="1.0">
                  <base href="https://example.com/new-base/">
                </head>
                <body>
                  <a href="link-1.0.tar.gz#sha256=0000000000000000000000000000000000000000000000000000000000000000">link1</a>
                  <a href="/elsewhere/link-2.0.zip" data-yanked="some reason">link2</a>
                  <a href="link-3.0.tar.gz" data-requires-python=">= 3.17">link3</a>
                </body>
              </html>
            "#;
    let url = Url::from_str("https://example.com/simple/link").unwrap();
    c.bench_with_input(
        BenchmarkId::new("parse_project_info", "html"),
        &(html, url),
        |b, (html, url)| b.iter(|| parse_project_info_html(url, html)),
    );
}

fn parse_package_names(c: &mut Criterion) {
    let html = r#"
        <html>
  <head>
    <meta name="pypi:repository-version" content="1.1">
    <title>Simple index</title>
  </head>
  <body>
    <a href="/simple/0/">0</a>
    <a href="/simple/0-0/">0-._.-._.-._.-._.-._.-._.-0</a>
    <a href="/simple/000/">000</a>
    <a href="/simple/0-0-1/">0.0.1</a>
    <a href="/simple/00101s/">00101s</a>
    <a href="/simple/00print-lol/">00print_lol</a>
    <a href="/simple/00smalinux/">00SMALINUX</a>
    <a href="/simple/0101/">0101</a>
    <a href="/simple/01changer/">01changer</a>
    <a href="/simple/01d61084-d29e-11e9-96d1-7c5cf84ffe8e/">01d61084-d29e-11e9-96d1-7c5cf84ffe8e</a>
    <a href="/simple/01-distributions/">01-distributions</a>
    <a href="/simple/021/">021</a>
    <a href="/simple/024travis-test024/">024travis-test024</a>
    <a href="/simple/02exercicio/">02exercicio</a>
    <a href="/simple/0411-test/">0411-test</a>
    <a href="/simple/0-618/">0.618</a>
    <a href="/simple/0706xiaoye/">0706xiaoye</a>
    <a href="/simple/0805nexter/">0805nexter</a>
    <a href="/simple/090807040506030201testpip/">090807040506030201testpip</a>
    <a href="/simple/0-core-client/">0-core-client</a>
    <a href="/simple/0fela/">0FELA</a>
    <a href="/simple/0html/">0html</a>
    <a href="/simple/0imap/">0imap</a>
    <a href="/simple/0lever-so/">0lever-so</a>
    <a href="/simple/0lever-utils/">0lever-utils</a>
    <a href="/simple/0-orchestrator/">0-orchestrator</a>
    <a href="/simple/0proto/">0proto</a>
    <a href="/simple/0rest/">0rest</a>
    <a href="/simple/0rss/">0rss</a>
    <a href="/simple/0wdg9nbmpm/">0wdg9nbmpm</a>
    <a href="/simple/0wneg/">0wneg</a>
    <a href="/simple/0x01-autocert-dns-aliyun/">0x01-autocert-dns-aliyun</a>
    <a href="/simple/0x01-cubic-sdk/">0x01-cubic-sdk</a>
    <a href="/simple/0x01-letsencrypt/">0x01-letsencrypt</a>
    <a href="/simple/0x0-python/">0x0-python</a>
    <a href="/simple/0x10c-asm/">0x10c-asm</a>
    <a href="/simple/0x20bf/">0x20bf</a>
    <a href="/simple/0x2nac0nda/">0x2nac0nda</a>
    <a href="/simple/0x-contract-addresses/">0x-contract-addresses</a>
    <a href="/simple/0x-contract-artifacts/">0x-contract-artifacts</a>
    <a href="/simple/0x-contract-wrappers/">0x-contract-wrappers</a>
   </body>
   </html>
        "#;

    c.bench_function("parse_package_names", |b| {
        b.iter(|| parse_package_names_html(black_box(html)))
    });
}

criterion_group!(benches, parse_project_info, parse_package_names);
criterion_main!(benches);
