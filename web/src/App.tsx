import { BrowserRouter, Route, Routes } from "react-router-dom";
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
                    <Route path="/setup" element={<SetupWizard />} />
                    <Route path="/login" element={<Login />} />
                    <Route path="/chat" element={<Chat />} />
                    <Route path="*" element={<AppBoot />} />
                </Routes>
            </BrowserRouter>
        </AuthProvider>
    );
}
