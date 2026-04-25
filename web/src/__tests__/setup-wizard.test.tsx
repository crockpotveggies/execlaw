import { describe, expect, it } from "vitest";
import { validateSetupForm } from "../routes/SetupWizard";

describe("validateSetupForm", () => {
    it("flags an empty display name", () => {
        const errs = validateSetupForm({
            display_name: "  ",
            admin_password: "hunter2-longer",
            email: "",
        });
        expect(errs.display_name).toBeTruthy();
        expect(errs.admin_password).toBeUndefined();
    });

    it("requires a password of at least 8 chars", () => {
        const errs = validateSetupForm({
            display_name: "Justin",
            admin_password: "short",
            email: "",
        });
        expect(errs.admin_password).toBeTruthy();
    });

    it("accepts an 8-char password (boundary)", () => {
        const errs = validateSetupForm({
            display_name: "Justin",
            admin_password: "12345678",
            email: "",
        });
        expect(errs.admin_password).toBeUndefined();
    });

    it("ignores an empty email", () => {
        const errs = validateSetupForm({
            display_name: "Justin",
            admin_password: "hunter2-longer",
            email: "",
        });
        expect(errs.email).toBeUndefined();
    });

    it("rejects a malformed email but accepts a normal one", () => {
        expect(
            validateSetupForm({
                display_name: "Justin",
                admin_password: "hunter2-longer",
                email: "not-an-email",
            }).email,
        ).toBeTruthy();
        expect(
            validateSetupForm({
                display_name: "Justin",
                admin_password: "hunter2-longer",
                email: "j@example.com",
            }).email,
        ).toBeUndefined();
    });

    it("returns no errors for a valid input", () => {
        expect(
            validateSetupForm({
                display_name: "Justin",
                admin_password: "hunter2-longer",
                email: "j@example.com",
            }),
        ).toEqual({});
    });
});
