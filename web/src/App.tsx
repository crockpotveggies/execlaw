import { BrowserRouter, Route, Routes } from "react-router-dom";
import { AuthProvider } from "./auth/AuthContext";
import { AppBoot } from "./routes/AppBoot";
import { Chat } from "./routes/Chat";
import { Login } from "./routes/Login";
import { RequireSetupComplete } from "./routes/RequireSetupComplete";
import { SetupWizard } from "./routes/SetupWizard";
import { Settings } from "./settings/Settings";

export function App() {
    return (
        <AuthProvider>
            <BrowserRouter>
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
                    <Route
                        path="/chat"
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
                    <Route path="*" element={<AppBoot />} />
                </Routes>
            </BrowserRouter>
        </AuthProvider>
    );
}
