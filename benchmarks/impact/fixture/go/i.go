package i

import . "example.com/impact/h"

func Check(n int) bool {
	return n < LIMIT
}

func Doubled() int {
	return LIMIT * 2
}
