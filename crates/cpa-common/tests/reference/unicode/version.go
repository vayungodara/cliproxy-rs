package main

import "runtime"

func goVersion() string { return runtime.Version() }
