import { RouterProvider } from "react-router-dom";

import { router } from "@/app/router";
import { ThemeProvider } from "@/theme/theme-provider";

function App() {
  return (
    <ThemeProvider>
      <RouterProvider router={router} />
    </ThemeProvider>
  );
}

export default App;
