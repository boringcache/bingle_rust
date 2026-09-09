# bingle_rust: BoringCache validation

Completed 9 September 2026. Fork-only validation; no outreach, upstream PR or comment was sent.

The original workflow reuses dependencies, but native diagnostics show that its own libraries miss again on a fresh runner while build timestamps change. Aggregate Rust hit rates therefore do not meet issue 151's workspace-crate criterion by themselves. A separate workflow input tests SOURCE_DATE_EPOCH without changing the original timing cohort. See the diagnostic table for library-level outcomes. Android validation covers the native library and bindings script, not Gradle, an emulator or full mobile E2E. The cdylib link is not claimed as a cacheable library hit.

## Measured workloads

Timings measure the selected build command. Cold/warm values have one sample per phase and provider. Rolling values are the median and range across five different upstream revisions. Queue time, setup, cache restoration and post-job saving are outside the workload timer; these steps are available in the JSON evidence.

| Workload | Provider | Cold | Fresh-runner warm | Five rolling changes: median (range) |
| --- | --- | ---: | ---: | ---: |
| android | BoringCache | 515.7 | 141.9 | 178.6 (146.7–225.2) |
| android | GitHub | 564.8 | 344.2 | 314.6 (311.3–428.3) |
| host | BoringCache | 190.6 | 79.1 | 78.7 (55.3–85.4) |
| host | GitHub | 284.1 | 196.2 | 164.5 (141.9–232.7) |

Qualification source: [upstream request](https://github.com/binglefoundation/bingle_rust/issues/151).

Ubuntu 24.04, Rust 1.98.1 and Android NDK 27.1.12297006. The host check builds and verifies the localnet provisioner executable. Android runs the upstream x86_64 native-library/bindings script and its leak check. The separate SOURCE_DATE_EPOCH diagnostic produced fresh-runner hits for bingle_core/bingle_test on the host and bingle_core/bingle_local for Android and host-side bindings. No extra timing improvement from that adjustment is claimed from these single pairs.

The GitHub seed recorded 1,281 native cache write errors on the host and 2,192 on Android. Its warm timings therefore reflect incomplete cache population, not a clean speed comparison against a successfully populated GitHub cache. The original runs did not retain native debug reasons for those failures. All five BoringCache rolling jobs in each case recorded zero native cache write errors.

## Source and integration

The captured source window starts at `8cf33ac3b8fa97b5c6902fe8a720322a1eb114dd` (captured upstream head ~5) and ends at `9349eb3f673f8a354077e52eb41dac4cfdbf84db`. Every adjacent first-parent patch was applied in order. Each job checks source equality against `.github/boringcache-source`, excluding only the validation harness.

The paired jobs use the same source, runner class, toolchain and cache surface. BoringCache One is pinned to `404b744a2053da4cf963f13f615f7fafe94f3cf7` (v1.30.1); actual CLI versions are retained. Authentication uses GitHub OIDC, with no static cache token. Cold and rolling jobs may publish; warm jobs restore only. Rust jobs use sccache 0.17.0 for both providers and no target/package cache. The cc crate can also use the Rust wrapper for native dependencies; Rust counts are reported separately.

| Change | Upstream revision | Subject |
| ---: | --- | --- |
| 1 | [dc846f7ca7b4](https://github.com/binglefoundation/bingle_rust/commit/dc846f7ca7b4b8369f10f3d0f8c96bf835618667) | Merge pull request #223 from binglefoundation/feat/216-store-and-forward-e2e |
| 2 | [e5759a2aab7a](https://github.com/binglefoundation/bingle_rust/commit/e5759a2aab7ae7dc7aca6e7490c9f03caaa7ab33) | Merge pull request #229 from binglefoundation/dependabot/github_actions/github-actions-31df092e2e |
| 3 | [4749511c110e](https://github.com/binglefoundation/bingle_rust/commit/4749511c110e1d1c4c8ff6e13145585f805f697d) | Merge pull request #228 from binglefoundation/feat/226-store-and-forward-testnet-e2e |
| 4 | [b3c4128ee747](https://github.com/binglefoundation/bingle_rust/commit/b3c4128ee74770b0121f86e5ff3c13f9484cdb8b) | Merge pull request #230 from binglefoundation/fix/algochainconfig-default-fields |
| 5 | [9349eb3f673f](https://github.com/binglefoundation/bingle_rust/commit/9349eb3f673f8a354077e52eb41dac4cfdbf84db) | Merge pull request #233 from binglefoundation/feat/231-delegate-opted-in-accounts-scan |

## Separate native diagnostics

These runs are excluded from the timing table above. Library attribution includes only workspace library targets. Native write-error counters do not alone identify a remote service failure.

| Experiment | Workload / phase | Workspace library lookups | Read-only write denials | Other write errors | Evidence |
| --- | --- | --- | ---: | ---: | --- |
| bingle_natural_metadata | host / cold | bingle_core: 0 hit / 1 miss; bingle_test: 0 hit / 1 miss | 0 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34354407009/job/102475315131) |
| bingle_natural_metadata | android / cold | bingle_core: 0 hit / 2 miss; bingle_local: 0 hit / 2 miss | 0 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34354407009/job/102475315266) |
| bingle_natural_metadata | android / warm | bingle_core: 0 hit / 2 miss; bingle_local: 0 hit / 2 miss | 8 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34354407009/job/102480397257) |
| bingle_natural_metadata | host / warm | bingle_core: 0 hit / 1 miss; bingle_test: 0 hit / 1 miss | 4 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34354407009/job/102480397510) |
| bingle_reproducible_metadata | android / cold | bingle_core: 0 hit / 2 miss; bingle_local: 0 hit / 2 miss | 0 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34355350222/job/102482554053) |
| bingle_reproducible_metadata | host / cold | bingle_core: 0 hit / 1 miss; bingle_test: 0 hit / 1 miss | 0 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34355350222/job/102482554568) |
| bingle_reproducible_metadata | host / warm | bingle_core: 1 hit / 0 miss; bingle_test: 1 hit / 0 miss | 1 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34355350222/job/102487341167) |
| bingle_reproducible_metadata | android / warm | bingle_core: 2 hit / 0 miss; bingle_local: 2 hit / 0 miss | 2 | 0 | [job](https://github.com/boringcache/bingle_rust/actions/runs/34355350222/job/102487341278) |

## Per-job evidence

[Measurements](boringcache-measurements.json) retain source hashes, timings, commands, test outcomes, native counters and final job links. [Source window](boringcache-source-window.json) retains the original revisions and changed paths. Actions artifacts have a 30-day retention setting; these committed summaries do not depend on artifact retention.

| Source index | Case / phase | Provider | Workload seconds | Job |
| ---: | --- | --- | ---: | --- |
| 0 | host / cold | BoringCache | 190.623 | [job 102467809893](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102467809893) |
| 0 | host / cold | GitHub | 284.054 | [job 102467810016](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102467810016) |
| 0 | android / cold | BoringCache | 515.668 | [job 102467810180](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102467810180) |
| 0 | android / cold | GitHub | 564.838 | [job 102467810240](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102467810240) |
| 0 | android / warm | BoringCache | 141.888 | [job 102471230944](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102471230944) |
| 0 | host / warm | BoringCache | 79.067 | [job 102471231009](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102471231009) |
| 0 | android / warm | GitHub | 344.227 | [job 102471231127](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102471231127) |
| 0 | host / warm | GitHub | 196.243 | [job 102471231226](https://github.com/boringcache/bingle_rust/actions/runs/34351973495/job/102471231226) |
| 1 | host / commit | GitHub | 232.665 | [job 102473883903](https://github.com/boringcache/bingle_rust/actions/runs/34353983750/job/102473883903) |
| 1 | android / commit | GitHub | 428.317 | [job 102473884234](https://github.com/boringcache/bingle_rust/actions/runs/34353983750/job/102473884234) |
| 1 | host / commit | BoringCache | 78.710 | [job 102473884275](https://github.com/boringcache/bingle_rust/actions/runs/34353983750/job/102473884275) |
| 1 | android / commit | BoringCache | 177.246 | [job 102473884397](https://github.com/boringcache/bingle_rust/actions/runs/34353983750/job/102473884397) |
| 2 | host / commit | BoringCache | 55.336 | [job 102477058869](https://github.com/boringcache/bingle_rust/actions/runs/34354926463/job/102477058869) |
| 2 | android / commit | BoringCache | 178.594 | [job 102477059123](https://github.com/boringcache/bingle_rust/actions/runs/34354926463/job/102477059123) |
| 2 | host / commit | GitHub | 153.153 | [job 102477059199](https://github.com/boringcache/bingle_rust/actions/runs/34354926463/job/102477059199) |
| 2 | android / commit | GitHub | 314.638 | [job 102477059251](https://github.com/boringcache/bingle_rust/actions/runs/34354926463/job/102477059251) |
| 3 | android / commit | BoringCache | 146.739 | [job 102479414927](https://github.com/boringcache/bingle_rust/actions/runs/34355618883/job/102479414927) |
| 3 | host / commit | BoringCache | 82.470 | [job 102479415121](https://github.com/boringcache/bingle_rust/actions/runs/34355618883/job/102479415121) |
| 3 | host / commit | GitHub | 141.929 | [job 102479415259](https://github.com/boringcache/bingle_rust/actions/runs/34355618883/job/102479415259) |
| 3 | android / commit | GitHub | 312.537 | [job 102479415323](https://github.com/boringcache/bingle_rust/actions/runs/34355618883/job/102479415323) |
| 4 | host / commit | BoringCache | 85.386 | [job 102481810990](https://github.com/boringcache/bingle_rust/actions/runs/34356323483/job/102481810990) |
| 4 | android / commit | BoringCache | 225.248 | [job 102481811174](https://github.com/boringcache/bingle_rust/actions/runs/34356323483/job/102481811174) |
| 4 | host / commit | GitHub | 164.530 | [job 102481811255](https://github.com/boringcache/bingle_rust/actions/runs/34356323483/job/102481811255) |
| 4 | android / commit | GitHub | 327.682 | [job 102481811277](https://github.com/boringcache/bingle_rust/actions/runs/34356323483/job/102481811277) |
| 5 | android / commit | BoringCache | 219.047 | [job 102484425652](https://github.com/boringcache/bingle_rust/actions/runs/34357094580/job/102484425652) |
| 5 | android / commit | GitHub | 311.258 | [job 102484425822](https://github.com/boringcache/bingle_rust/actions/runs/34357094580/job/102484425822) |
| 5 | host / commit | GitHub | 171.014 | [job 102484425830](https://github.com/boringcache/bingle_rust/actions/runs/34357094580/job/102484425830) |
| 5 | host / commit | BoringCache | 71.427 | [job 102484425870](https://github.com/boringcache/bingle_rust/actions/runs/34357094580/job/102484425870) |


## Full integration follow-up

The [Cargo/Docker integration report](boringcache-full-validation.md) records the subsequent first-class Cargo and relevant Docker validation. The compiler/archive results above remain a separate cohort; use the preserved `compiler-cache-validation` branch to reproduce that configuration.
