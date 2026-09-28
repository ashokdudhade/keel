package m

import "example.com/impact/l"

func GetPort(c l.Config) int {
	return c.Port
}

func HasPort(c l.Config) bool {
	return c.Port > 0
}
