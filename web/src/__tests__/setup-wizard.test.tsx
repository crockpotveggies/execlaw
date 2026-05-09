import { describe, expect, it } from "vitest";
import { validateSetupForm } from "../routes/SetupWizard";

const valid = {
    username: "jlong",
    display_name: "Justin",
    admin_password: "hunter2-longer",
    email: "",
};

describe("validateSetupForm", () => {
    it("flags an empty display name", () => {
        const errs = validateSetupForm({ ...valid, display_name: "  " });
        expect(errs.display_name).toBeTruthy();
        expect(errs.admin_password).toBeUndefined();
    });

    it("requires a password of at least 8 chars", () => {
        const errs = validateSetupForm({ ...valid, admin_password: "short" });
        expect(errs.admin_password).toBeTruthy();
    });

    it("accepts an 8-char password (boundary)", () => {
        const errs = validateSetupForm({ ...valid, admin_password: "12345678" });
        expect(errs.admin_password).toBeUndefined();
    });

    it("ignores an empty email", () => {
        const errs = validateSetupForm({ ...valid, email: "" });
        expect(errs.email).toBeUndefined();
    });

    it("rejects a malformed email but accepts a normal one", () => {
        expect(
            validateSetupForm({ ...valid, email: "not-an-email" }).email,
        ).toBeTruthy();
        expect(
            validateSetupForm({ ...valid, email: "j@example.com" }).email,
        ).toBeUndefined();
    });

    it("returns no errors for a valid input", () => {
        expect(
            validateSetupForm({ ...valid, email: "j@example.com" }),
        ).toEqual({});
    });

    // ---- username -------------------------------------------------------

    it("requires a username", () => {
        expect(
            validateSetupForm({ ...valid, username: "" }).username,
        ).toBeTruthy();
        expect(
            validateSetupForm({ ...valid, username: "   " }).username,
        ).toBeTruthy();
    });

    it("rejects too-short / too-long usernames", () => {
        expect(
            validateSetupForm({ ...valid, username: "ab" }).username,
        ).toBeTruthy();
        // Boundary: 3 chars accepted.
        expect(
            validateSetupForm({ ...valid, username: "abc" }).username,
        ).toBeUndefined();
        // Boundary: 32 chars accepted.
        expect(
            validateSetupForm({ ...valid, username: "a".repeat(32) }).username,
        ).toBeUndefined();
        // 33 chars rejected.
        expect(
            validateSetupForm({ ...valid, username: "a".repeat(33) }).username,
        ).toBeTruthy();
    });

    it("rejects usernames with disallowed characters", () => {
        for (const bad of ["has space", "j@l", "with.dot", "with/slash"]) {
            expect(
                validateSetupForm({ ...valid, username: bad }).username,
            ).toBeTruthy();
        }
    });

    it("accepts hyphen + underscore + digits", () => {
        for (const ok of ["j_long", "j-long", "user42", "abc-123_xyz"]) {
            expect(
                validateSetupForm({ ...valid, username: ok }).username,
            ).toBeUndefined();
        }
    });
});
