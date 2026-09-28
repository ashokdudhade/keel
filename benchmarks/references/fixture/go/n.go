package n

func Helper(x int) int {
	return -x
}

func Other(x int) int {
	return Helper(x)
}
