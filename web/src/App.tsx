import { BrowserRouter, Route, Routes } from "react-router-dom";
import { ScreenTransition } from "./anim/ScreenTransition";
import { AuthProvider } from "./auth/AuthContext";
import { AppBoot } from "./routes/AppBoot";
import { Chat } from "./routes/Chat";
import { Login } from "./routes/Login";
import { SetupWizard } from "./routes/SetupWizard";

export function App() {
    return (
        <AuthProvider>
            <BrowserRouter>
                <Routes>
                    <Route path="/" element={<AppBoot />} />
                    <Route
                        path="/setup"
                        element={
                            <ScreenTransition>
                                <SetupWizard />
                            </ScreenTransition>
                        }
                    />
                    <Route
                        path="/login"
                        element={
                            <ScreenTransition>
                                <Login />
                            </ScreenTransition>
                        }
                    />
                    <Route
                        path="/chat"
                        element={
                            <ScreenTransition>
                                <Chat />
                            </ScreenTransition>
                        }
                    />
                    <Route path="*" element={<AppBoot />} />
                </Routes>
            </BrowserRouter>
        </AuthProvider>
    );
}
