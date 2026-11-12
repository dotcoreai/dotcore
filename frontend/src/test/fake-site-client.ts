/** A site client's console settings update for tests that never change the settings. */
export const rejectConsoleSettingsUpdate = () =>
  Promise.reject(new Error("updateConsoleSettings is not faked in this test"));
