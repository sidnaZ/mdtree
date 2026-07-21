# Third-party notices

MDTree is licensed under the [GNU Affero General Public License v3.0](LICENSE)
(or a separate [commercial license](COMMERCIAL-LICENSE.md)) and depends on
open-source Rust crates. Direct runtime and development dependencies at the
0.1.0 lockfile are listed below; transitive versions are fixed in
`Cargo.lock`. Their own license files and source distributions remain
authoritative.

| Crate | Version requirement | License expression |
| --- | --- | --- |
| anyhow | 1.0.103 | MIT OR Apache-2.0 |
| blake3 | 1.8.5 | CC0-1.0 OR Apache-2.0 |
| clap | 4.6.1 | MIT OR Apache-2.0 |
| criterion | 0.7.0 | MIT OR Apache-2.0 |
| pulldown-cmark | 0.13.4 | MIT |
| rmcp | 2.2.0 | Apache-2.0 |
| rusqlite | 0.40.1 | MIT |
| serde / serde_json / serde_yaml | lockfile | MIT OR Apache-2.0 |
| tempfile | 3.23+ | MIT OR Apache-2.0 |
| thiserror | 2.0.18 | MIT OR Apache-2.0 |
| tokio | 1.52.3 | MIT |
| tracing / tracing-subscriber | lockfile | MIT |
| ulid | 2.0.1 | MIT |
| unicode-normalization | 0.1.25 | MIT OR Apache-2.0 |

The bundled SQLite library is public domain. This notice is informational and
does not replace the license terms distributed by each dependency.
