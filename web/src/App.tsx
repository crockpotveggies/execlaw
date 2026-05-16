import { BrowserRouter, Route, Routes } from "react-router-dom";
import { AuthProvider } from "./auth/AuthContext";
import { BridgeInstaller } from "./plugins/BridgeInstaller";
import { AlertWatcher } from "./routes/AlertWatcher";
import { ApprovalWatcher } from "./routes/ApprovalWatcher";
import { AppBoot } from "./routes/AppBoot";
import { Approvals } from "./routes/Approvals";
import { Chat } from "./routes/Chat";
import { ConnectionBanner } from "./routes/ConnectionBanner";
import { Login } from "./routes/Login";
import { RequireSetupComplete } from "./routes/RequireSetupComplete";
import { Research } from "./routes/Research";
import { Routines } from "./routes/Routines";
import { SetupWizard } from "./routes/SetupWizard";
import { Skills } from "./routes/Skills";
import { Settings } from "./settings/Settings";

export function App() {
    return (
        <AuthProvider>
            <BrowserRouter>
                {/*
                  Installs `globalThis.execlawHost` so dynamically-
                  loaded plugin UI panels can reach the host's React +
                  helpers + shared components through a single stable
                  global. Idempotent + headless. See
                  `plugins/host-bridge.ts` for the bridge contract and
                  `plugins/types.ts` for the API plugin authors consume.
                */}
                <BridgeInstaller />
                <ConnectionBanner />
                {/*
                  Persistent alert-event WS subscription. Lives at the
                  App level (NOT inside any per-route component) so the
                  brand-status indicator next to the execlaw logo
                  updates in real-time on every route — without it the
                  alert count only refreshed via the Sidebar's 60s
                  poll once the operator left /chat (where Chat.tsx's
                  own WS used to be the only one). Headless — see
                  routes/AlertWatcher.tsx for the rationale.
                */}
                <AlertWatcher />
                {/*
                  Same shape as AlertWatcher, for the sidebar's
                  pending-approvals badge. A cold contact landing
                  while the operator is on Settings / Research /
                  Routines used to stay invisible until the next
                  navigation or hard refresh — the badge only
                  refreshed on Sidebar mount. ApprovalWatcher's WS
                  subscription re-syncs the canonical list on
                  `approval_created` / `approval_resolved`.
                */}
                <ApprovalWatcher />
                <Routes>
                    <Route path="/" element={<AppBoot />} />
                    <Route path="/setup" element={<SetupWizard />} />
                    <Route path="/login" element={<Login />} />
                    {/*
                      Chat + Settings are the post-setup routes. The
                      guard re-probes /api/ping on mount so a deep
                      link past an unfinished wizard bounces the
                      operator back to /setup instead of dropping
                      them into a broken chat shell with no inference
                      backend (Phase 14 follow-up).
                    */}
                    {/*
                      `/chat` shows the welcome view; `/chat/:id`
                      activates a specific thread. Both go through
                      the same Chat shell so deep-linking
                      (refresh-tolerant or sharable per-thread URLs)
                      works without re-mounting the WebSocket /
                      sidebar tree.
                    */}
                    <Route
                        path="/chat"
                        element={
                            <RequireSetupComplete>
                                <Chat />
                            </RequireSetupComplete>
                        }
                    />
                    <Route
                        path="/chat/:conversationId"
                        element={
                            <RequireSetupComplete>
                                <Chat />
                            </RequireSetupComplete>
                        }
                    />
                    <Route
                        path="/settings/*"
                        element={
                            <RequireSetupComplete>
                                <Settings />
                            </RequireSetupComplete>
                        }
                    />
                    <Route
                        path="/routines"
                        element={
                            <RequireSetupComplete>
                                <Routines />
                            </RequireSetupComplete>
                        }
                    />
                    <Route
                        path="/research"
                        element={
                            <RequireSetupComplete>
                                <Research />
                            </RequireSetupComplete>
                        }
                    />
                    <Route
                        path="/research/:jobId"
                        element={
                            <RequireSetupComplete>
                                <Research />
                            </RequireSetupComplete>
                        }
                    />
                    <Route
                        path="/skills"
                        element={
                            <RequireSetupComplete>
                                <Skills />
                            </RequireSetupComplete>
                        }
                    />
                    <Route
                        path="/approvals"
                        element={
                            <RequireSetupComplete>
                                <Approvals />
                            </RequireSetupComplete>
                        }
                    />
                    <Route path="*" element={<AppBoot />} />
                </Routes>
            </BrowserRouter>
        </AuthProvider>
    );
}
