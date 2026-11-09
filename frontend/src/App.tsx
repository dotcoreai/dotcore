import { RouterProvider } from "react-router-dom";

import { createConsoleClients } from "@/api/clients";
import { ClientsProvider } from "@/api/clients-provider";
import { router } from "@/app/router";
import { ThemeProvider } from "@/theme/theme-provider";

const clients = createConsoleClients();

function App() {
  return (
    <ThemeProvider>
      <ClientsProvider clients={clients}>
        <RouterProvider router={router} />
      </ClientsProvider>
    </ThemeProvider>
  );
}

export default App;
