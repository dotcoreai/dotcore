import userEvent, { type UserEvent } from "@testing-library/user-event";

/**
 * Sets up user-event without the pause between actions, which would otherwise yield to timers and
 * the event loop after every keystroke. Tests that wait for a result do so on observable state.
 */
export function setupUser(): UserEvent {
  return userEvent.setup({ delay: null });
}
