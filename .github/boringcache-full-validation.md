# Bingle: Cargo and Docker integration validation

Measured on 9 September 2026. This fork follows the [adorsys status-list-server integration](https://github.com/boringcache/status-list-server/tree/refs/heads/boringcache-validation): a repository-owned `.boringcache.toml` plan, thin One workflow steps, OIDC authentication, trusted writers and read-only consumers.

The runs use released One/CLI 1.30.1 and sccache 0.17.0. One is pinned to `404b744a2053da4cf963f13f615f7fafe94f3cf7`. No unreleased CLI changes were used. Whole-job times exclude queue time; One elapsed includes its own cache operations and wrapped command. These timing boundaries must not be mixed.

The host uses `mode: cargo`. The existing Android build script has an opt-in `BORINGCACHE_CARGO_POLICY` switch and runs both Android release compilation and host binding generation through first-class Cargo. The original command remains available when that switch is unset. The Android profile keeps its existing `/var/tmp/bingle_native_target` output path. Java 17, NDK 27.1.12297006, Rust 1.98.1 and the x86_64 Android target are pinned. Library validation, binding generation and path-leak checks remain in place. No emulator, Gradle or Android end-to-end result is claimed.

The first Android build succeeded and published its target archive, but the job's One archive post-step failed because an unused `~/.cargo/git/db` directory was requested. Removing that unused profile entry fixed the full job. The [initial failed workflow](https://github.com/boringcache/bingle_rust/actions/runs/34362432924) is retained as a failed attempt; the corrected seed is explicitly not described as cold.

In the corrected warm Android job, both compilation phases recorded two Rust hits and zero misses: four cacheable workspace compilations reused in total. The release phase took 10.27 s and the binding-generation phase 6.95 s. In the corrected host warm job, native evidence recorded one Rust hit, one miss and one write error; the whole job was slower than its already-warm seed. That earlier error was not reproduced in the subsequent diagnostic and its exact cause remains unassigned.

The [host diagnostic](https://github.com/boringcache/bingle_rust/actions/runs/34365480083) at `4fbb34df189a8d78a902b718903aea82027cdcd5` passed both jobs. Its warm job proved a hit for `bingle_core` and a hit for `bingle_test`, with zero misses and zero cache/read/write errors or timeouts. One Cargo elapsed was 54.9 s and whole-job time was 94 s. Source-based timestamps were identical within the pair. Build metadata and derived per-crate diagnostics are retained; raw debug logs are not published as artifacts.

That diagnostic also exposed archive scope growth: the native unit tests and localnet host command shared the same target tag, so the host downloaded an 8.51 GB archive. The final configuration separates localnet, unit-test and Android target profiles. Compiler storage and dependency archives remain shared where compatible. This avoids downloading unit-test outputs for the smaller localnet workload.

The [isolated localnet profile validation](https://github.com/boringcache/bingle_rust/actions/runs/34366680001) at `9499c7976f9b8005ff830a4cfd93a95228177bce` passed both jobs. Its new target archive was 2.74 GB, compared with the previous mixed 8.51 GB archive; these are archive content-size displays, not wire-byte or unique-storage measurements. The new seed had a target miss but an already-populated compiler cache. Whole jobs took 301 s and 98 s; One Cargo took 263.8 s and 56.4 s. Warm Cargo compilation took 7.83 s, with `bingle_core` and `bingle_test` both hitting and zero cache/read/write errors or timeouts. The seed recorded one unattributed native cache error and zero read/write errors or timeouts. This is a successful isolated-profile validation, not a completely cold backend pair.

The [original native unit workflow validation](https://github.com/boringcache/bingle_rust/actions/runs/34364365538) passed. The [native unit validation after profile separation](https://github.com/boringcache/bingle_rust/actions/runs/34367558752) also passed, in 294 s at `9499c7976f9b8005ff830a4cfd93a95228177bce`.


## Evidence and limits

The [full integration measurements](boringcache-full-measurements.json) retain workflow and job URLs, exact configuration SHAs, step timings, native tool statistics and relevant log observations. Failed and canceled attempts remain in the evidence. Successful job status alone is not treated as proof of complete cache reuse or zero cache errors.

The earlier [compiler/archive comparison](https://github.com/boringcache/bingle_rust/blob/compiler-cache-validation/.github/boringcache-validation.md) starts at the captured upstream HEAD~5 and tests five real first-parent changes. It is a separate cohort and must not be relabeled as full Cargo/Docker measurements. Its historical workflows should be dispatched from `compiler-cache-validation`; their strict source-equality checks intentionally reject later integration changes. The captured source window remains in `boringcache-source-window.json` beside this report.

These observations do not establish long-term retention, eviction resilience, unique-storage or cost savings, or a matched full-pipeline speed improvement over upstream. No upstream pull request, GitHub comment or outreach message was sent by this validation task.
