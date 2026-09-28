package k

import . "example.com/impact/j"

func Describe(m string) string {
	switch m {
	case Mode:
		return Mode
	default:
		return "slow"
	}
}

func IsDefault(m string) bool {
	choice := Mode
	return m == choice
}
