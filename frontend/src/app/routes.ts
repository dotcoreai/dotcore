import { createElement } from "react";
import { Navigate } from "react-router-dom";

import { ConsoleLayout } from "@/app/console-layout";
import { AppCatalogPage } from "@/features/applications/app-catalog-page";
import { ApplicationsPage } from "@/features/applications/applications-page";
import { ApplicationsSection } from "@/features/applications/applications-section";
import { ChatPage } from "@/features/chat/chat-page";
import { RoutesPage } from "@/features/inference-routes/routes-page";
import { MachinesPage } from "@/features/machines/machines-page";
import { ModelCatalogPage } from "@/features/models/model-catalog-page";
import { ModelsPage } from "@/features/models/models-page";
import { OverviewPage } from "@/features/overview/overview-page";
import { PreferencesPage } from "@/features/preferences/preferences-page";

/** The console's route table: the console layout around every feature page. */
export const routes = [
  {
    path: "/",
    element: createElement(Navigate, { replace: true, to: "/overview" }),
  },
  {
    element: createElement(ConsoleLayout),
    children: [
      {
        path: "/overview",
        element: createElement(OverviewPage),
      },
      {
        path: "/models",
        children: [
          {
            index: true,
            element: createElement(ModelsPage),
          },
          {
            path: "catalog",
            element: createElement(ModelCatalogPage),
          },
        ],
      },
      {
        path: "/routes",
        element: createElement(RoutesPage),
      },
      {
        path: "/chat",
        element: createElement(ChatPage),
      },
      {
        path: "/machines",
        element: createElement(MachinesPage),
      },
      {
        path: "/preferences",
        element: createElement(PreferencesPage),
      },
      {
        path: "/apps",
        element: createElement(ApplicationsSection),
        children: [
          {
            index: true,
            element: createElement(ApplicationsPage),
          },
          {
            path: "catalog",
            element: createElement(AppCatalogPage),
          },
        ],
      },
    ],
  },
];
