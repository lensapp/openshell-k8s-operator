# Changelog

## [0.2.0](https://github.com/lensapp/openshell-k8s-operator/compare/v0.1.0...v0.2.0) (2026-07-21)


### Features

* add lease-based leader election ([042a3c7](https://github.com/lensapp/openshell-k8s-operator/commit/042a3c7bd09aadc5361868e829e9a201ba0d093b))
* add liveness and readiness probes ([d370390](https://github.com/lensapp/openshell-k8s-operator/commit/d370390feda3473f2fc9b4d508ff3ce22b88be02))
* add operator-provisioned persistent volumes to OpenShellSandbox ([2db40d4](https://github.com/lensapp/openshell-k8s-operator/commit/2db40d4466c99942d99210c524eb679e89f23107))
* add Policy CRD applied to sandboxes via policyRef ([d504795](https://github.com/lensapp/openshell-k8s-operator/commit/d504795265695d9beefad2c6e4288dae830412ea))
* add Provider CRD with Secret-backed credentials ([70d79e9](https://github.com/lensapp/openshell-k8s-operator/commit/70d79e914fc0c8c0bcd4410d4e3ab2ad466b28aa))
* add static OIDC issuer crate (mint + serve) ([023b4d6](https://github.com/lensapp/openshell-k8s-operator/commit/023b4d63a08df45714e86b6cc80e3620ad495e00))
* authenticate operator to gateway with OIDC bearer ([201c800](https://github.com/lensapp/openshell-k8s-operator/commit/201c800d4f61cd89b021351d36c39cc8056e2383))
* auto-select credential handling tier per provider ([daa4fde](https://github.com/lensapp/openshell-k8s-operator/commit/daa4fde977ff575c89026ad118d8220fd1401692))
* bundle the OpenShell gateway by default ([76c528a](https://github.com/lensapp/openshell-k8s-operator/commit/76c528a6351f39ca9321a25c1bcd5edee9c1c4d4))
* converge sandbox policy mutable fields in place ([c0b9155](https://github.com/lensapp/openshell-k8s-operator/commit/c0b91550ede55e66a971798584ad01f22b9cea15))
* converge sandbox providers in place via attach/detach ([bd7f411](https://github.com/lensapp/openshell-k8s-operator/commit/bd7f411eb46abc581f18b069b34e1259c485a803))
* expose cpu/memory, gpu count, runtime class, labels, annotations, log level ([9e1f07f](https://github.com/lensapp/openshell-k8s-operator/commit/9e1f07f77ed70865b54318968afe7cdd7e7c2bb3))
* package the operator for deployment (image, Helm chart, CI) ([67b67ec](https://github.com/lensapp/openshell-k8s-operator/commit/67b67ec48a30fd69d0e3152bbd230aef09a1dfe8))
* poll faster while a sandbox phase is unsettled ([576c4aa](https://github.com/lensapp/openshell-k8s-operator/commit/576c4aaeae5ea2e39ce5594df20c98d589095337))
* prefix CRD kinds and allow inline sandbox policy ([a14681a](https://github.com/lensapp/openshell-k8s-operator/commit/a14681a9efec70ea8829eaf3f5a53532a67f2027))
* recreate sandbox on immutable-field drift, reattaching volumes ([44e4c47](https://github.com/lensapp/openshell-k8s-operator/commit/44e4c47f1caddbdfa3df970d0507d992234023fa))
* report reconcile health via standard status conditions and events ([22d1fa0](https://github.com/lensapp/openshell-k8s-operator/commit/22d1fa0b469f3ed3cf2da04339dba727b745745b))
* scaffold Milestone 1 — OpenShellSandbox operator ([efe9f46](https://github.com/lensapp/openshell-k8s-operator/commit/efe9f4659e80b0f308ebd11a35e447e278889505))
* wire the bundled OIDC issuer into the Helm chart ([e8aa6a1](https://github.com/lensapp/openshell-k8s-operator/commit/e8aa6a178eaa26a2db7d383d8b0b57d260080bc0))


### Bug Fixes

* correct GitHub org from lenshq to lensapp ([776da65](https://github.com/lensapp/openshell-k8s-operator/commit/776da65f7e5252549f908e22109d3445c44d214e))
* correct pod security context and mint RBAC for live install ([d1be2bf](https://github.com/lensapp/openshell-k8s-operator/commit/d1be2bf3ec8aa15cd48dad57a350a3742f3ecbe9))
* reduce cognitive complexity of converge and cleanup ([fbdc908](https://github.com/lensapp/openshell-k8s-operator/commit/fbdc908ce0252f3379d870690eb74167fa9391f3))
