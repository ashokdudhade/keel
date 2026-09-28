package a

import "example.com/refs/m"

// Helper is great
var note = "call Helper"

func Run(x int) int {
	return m.Helper(x)
}
