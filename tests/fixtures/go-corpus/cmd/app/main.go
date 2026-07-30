// The CALLER side of the corpus. Everything reached from here crosses a package boundary, so a
// resolved edge out of this file proves cross-FILE binding rather than same-file name lookup.
package main

import (
	"fmt"

	"example.com/gocorpus/corekit"
)

// Run is the caller the corpus test asserts on. It reaches `corekit.Compute`, whose name is
// unique corpus-wide, so the resolver binds it by bare name with nothing to guess between.
func Run(seed int) int {
	return corekit.Compute(seed)
}

// UseCounter reaches the POINTER-receiver method through a package-qualified constructor. The
// method symbol is stored receiver-qualified (`Counter.Increment`) while the call site only
// carries the bare field `Increment`, so this call is what pins the honest baseline: the
// tree-sitter resolver has no receiver TYPE and must leave it unresolved rather than guess.
func UseCounter() int {
	counter := corekit.Counter{}
	counter.Increment(3)
	return counter.Total()
}

// Summarize calls a second unique cross-package function, so the corpus has more than one
// resolvable cross-file call and a single lucky bind cannot carry the assertion.
func Summarize() string {
	return corekit.Describe(corekit.Counter{})
}

func main() {
	fmt.Println(Run(21), UseCounter(), Summarize())
}
