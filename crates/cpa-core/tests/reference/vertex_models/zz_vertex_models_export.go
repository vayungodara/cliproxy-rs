package auth

// Overlaid into sdk/cliproxy/auth so the sdk/cliproxy recorder can reach the
// unexported execution path. Not a test file: package tests are not visible to
// other packages.

// ZZExecutionModels returns Go's upstream model candidates for one credential.
func (m *Manager) ZZExecutionModels(auth *Auth, routeModel string) []string {
	candidates, _, _, _ := m.executionModelCandidatesWithAlias(auth, routeModel)
	return candidates
}
