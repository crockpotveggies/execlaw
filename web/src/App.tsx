import { BrowserRouter, Route, Routes } from "react-router-dom";
import { AuthProvider } from "./auth/AuthContext";
import { AppBoot } from "./routes/AppBoot";
import { Chat } from "./routes/Chat";
import { ConnectionBanner } from "./routes/ConnectionBanner";
import { Login } from "./routes/Login";
import { RequireSetupComplete } from "./routes/RequireSetupComplete";
import { Research } from "./routes/Research";
import { Routines } from "./routes/Routines";
import { SetupWizard } from "./routes/SetupWizard";
import { Settings } from "./settings/Settings";

export function App() {
    return (
        <AuthProvider>
            <BrowserRouter>
                <ConnectionBanner />
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
                    <Route path="*" element={<AppBoot />} />
                </Routes>
            </BrowserRouter>
        </AuthProvider>
    );
}
