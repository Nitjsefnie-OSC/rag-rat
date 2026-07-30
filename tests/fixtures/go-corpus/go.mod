// The Go corpus (T8): a real two-package module, not loose .go files, so that a
// cross-PACKAGE call genuinely exists for the resolver to bind.
//
// `cmd/app` imports `corekit`; `corekit` imports nothing first-party. That direction is what
// makes `corekit.Compute(...)` a cross-file, cross-package call rather than a same-file one.
module example.com/gocorpus

go 1.22
