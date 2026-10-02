// Dumps Go's unicode.CaseRanges, caseOrbit and strconv.IsPrint ranges as Rust source.
package main

import (
	"fmt"
	"os"
	"strconv"
	"unicode"
)

func main() {
	f := os.Stdout
	fmt.Fprintln(f, "// @generated from Go "+goVersion()+" unicode.CaseRanges, unicode caseOrbit and strconv.IsPrint")
	fmt.Fprintln(f, "// by tests/reference/unicode/main.go. Do not edit.")
	fmt.Fprintln(f, "")
	fmt.Fprintln(f, "/// `unicode.CaseRanges`: (lo, hi, upper delta, lower delta, title delta); `UPPER_LOWER` marks alternating ranges.")
	fmt.Fprintf(f, "pub(crate) static CASE_RANGES: [(u32, u32, i32, i32, i32); %d] = [\n", len(unicode.CaseRanges))
	for _, r := range unicode.CaseRanges {
		fmt.Fprintf(f, "    (0x%04X, 0x%04X, %d, %d, %d),\n", r.Lo, r.Hi, r.Delta[0], r.Delta[1], r.Delta[2])
	}
	fmt.Fprintln(f, "];")
	// caseOrbit: runes whose SimpleFold is not the CaseRanges lower/upper rule.
	var orbit [][2]rune
	for r := rune(0); r <= unicode.MaxRune; r++ {
		want := r
		if l := unicode.ToLower(r); l != r {
			want = l
		} else {
			want = unicode.ToUpper(r)
		}
		if got := unicode.SimpleFold(r); got != want {
			orbit = append(orbit, [2]rune{r, got})
		}
	}
	fmt.Fprintf(f, "\n/// `unicode.SimpleFold` results that differ from `lower if different, else upper` (asciiFold and caseOrbit).\npub(crate) static CASE_ORBIT: [(u32, u32); %d] = [\n", len(orbit))
	for _, o := range orbit {
		fmt.Fprintf(f, "    (0x%04X, 0x%04X),\n", o[0], o[1])
	}
	fmt.Fprintln(f, "];")
	var ranges [][2]rune
	for r := rune(0); r <= unicode.MaxRune; r++ {
		if !strconv.IsPrint(r) {
			continue
		}
		if n := len(ranges); n > 0 && ranges[n-1][1] == r-1 {
			ranges[n-1][1] = r
		} else {
			ranges = append(ranges, [2]rune{r, r})
		}
	}
	fmt.Fprintf(f, "\n/// Printable rune ranges per `strconv.IsPrint`.\npub(crate) static PRINT_RANGES: [(u32, u32); %d] = [\n", len(ranges))
	for _, r := range ranges {
		fmt.Fprintf(f, "    (0x%04X, 0x%04X),\n", r[0], r[1])
	}
	fmt.Fprintln(f, "];")
}
