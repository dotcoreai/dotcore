// Read and minimally decode the persisted console appearance preference, then apply the document dark class and color scheme synchronously before application modules and CSS can paint.
// This prevents a flash of the saved theme.

(() => {
  const storageKey = "dotcore.console.preferences.v1";

  function readVarint(bytes, offset) {
    let value = 0;
    let shift = 0;

    while (offset < bytes.length && shift <= 49) {
      const byte = bytes[offset];
      offset += 1;
      value += (byte & 0x7f) * 2 ** shift;

      if ((byte & 0x80) === 0) {
        return [value, offset];
      }

      shift += 7;
    }

    return undefined;
  }

  function readStoredColorScheme() {
    try {
      const value = localStorage.getItem(storageKey);
      if (!value) {
        return undefined;
      }

      const binary = atob(value);
      const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0));
      let colorScheme = 0;
      let offset = 0;

      while (offset < bytes.length) {
        const tag = readVarint(bytes, offset);
        if (!tag) {
          return undefined;
        }

        const [fieldNumberAndWireType, nextOffset] = tag;
        const fieldNumber = Math.floor(fieldNumberAndWireType / 8);
        const wireType = fieldNumberAndWireType % 8;
        offset = nextOffset;

        if (fieldNumber === 0) {
          return undefined;
        }

        if (wireType === 0) {
          const value = readVarint(bytes, offset);
          if (!value) {
            return undefined;
          }

          offset = value[1];
          if (fieldNumber === 1) {
            colorScheme = value[0];
          }
          continue;
        }

        if (wireType === 1) {
          offset += 8;
        } else if (wireType === 2) {
          const length = readVarint(bytes, offset);
          if (!length) {
            return undefined;
          }

          offset = length[1] + length[0];
        } else if (wireType === 5) {
          offset += 4;
        } else {
          return undefined;
        }

        if (offset > bytes.length) {
          return undefined;
        }
      }

      return colorScheme === 1 ? "light" : colorScheme === 2 ? "dark" : undefined;
    } catch {
      return undefined;
    }
  }

  const isDark = readStoredColorScheme() !== "light";
  document.documentElement.classList.toggle("dark", isDark);
  document.documentElement.style.colorScheme = isDark ? "dark" : "light";
})();
