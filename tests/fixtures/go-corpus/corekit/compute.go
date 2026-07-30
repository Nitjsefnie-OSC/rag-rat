// Package corekit is the CALLEE side of the corpus: it declares the function and the
// pointer-receiver method that `cmd/app` reaches across the package boundary.
package corekit

import "fmt"

// Compute has a name that is UNIQUE across the whole corpus. That uniqueness is the point:
// bare-name resolution can only be asserted as correct when exactly one candidate exists, so a
// second `Compute` anywhere would turn a resolved edge into an (equally correct) refusal.
func Compute(value int) int {
	return value * 2
}

// Counter is the struct whose method is declared with a POINTER receiver below.
type Counter struct {
	total int
}

// Increment is the pointer-receiver method. Go's backend names this symbol `Counter.Increment`
// (receiver-qualified), which is what the corpus test pins.
func (c *Counter) Increment(by int) {
	c.total = c.total + by
}

// Total is a VALUE-receiver method on the same struct, so the corpus covers both receiver forms.
func (c Counter) Total() int {
	return c.total
}

// Describe exercises a call to an external (stdlib) package from the callee side, so the corpus
// contains an import that must NOT be mistaken for a first-party one.
func Describe(c Counter) string {
	return fmt.Sprintf("total=%d", c.Total())
}
