/** A local time with seconds, which tells attempts of the same minute apart. */
export function formatTime(date: Date): string {
  return date.toLocaleTimeString([], {
    hour: "2-digit",
    hourCycle: "h23",
    minute: "2-digit",
    second: "2-digit",
  });
}

/** A local time of day without seconds. */
export function formatClock(date: Date): string {
  return date.toLocaleTimeString([], { hour: "2-digit", hourCycle: "h23", minute: "2-digit" });
}

/** The host of a connection's address, without the port the connection came from. */
export function peerHost(address: string): string {
  return address.replace(/:\d+$/, "").replace(/^\[(.*)\]$/, "$1");
}
