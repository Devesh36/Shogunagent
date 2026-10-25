// First-time desktop Google OAuth setup. Rotation belongs in SHOGUN Settings.
import Foundation
import Security
import Darwin

func fail(_ message: String) -> Never {
    fputs(message + "\n", stderr)
    exit(1)
}
func query(service: String, account: String) -> [String: Any] {
    [kSecClass as String: kSecClassGenericPassword,
     kSecAttrService as String: service, kSecAttrAccount as String: account]
}
func requireFreshSetup() {
    // Include the legacy Keychain namespace; never silently supersede an old grant.
    for service in ["com.selectkk.shogun", "SHOGUN"] {
        for account in ["google-oauth-client-id", "google-oauth-client-secret", "gmail-tokenset", "gcal-tokenset", "gdrive-tokenset"] {
            let status = SecItemCopyMatching(query(service: service, account: account) as CFDictionary, nil)
            if status == errSecSuccess { fail("Google credentials already exist. Change them in SHOGUN Settings > Connections > Google OAuth.") }
            if status != errSecItemNotFound { fail("Cannot check existing Google credentials. Unlock Keychain and retry.") }
        }
    }
}
func addSecret(account: String, value: String) {
    var item = query(service: "com.selectkk.shogun", account: account)
    item[kSecValueData as String] = Data(value.utf8)
    item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
    if SecItemAdd(item as CFDictionary, nil) != errSecSuccess {
        fail("Google setup incomplete. Finish or clear the client in SHOGUN Settings > Connections > Google OAuth.")
    }
}
if isatty(STDIN_FILENO) != 1 { fail("Run google:setup in your terminal; interactive input required.") }
requireFreshSetup()
print("First-time Gmail / Google Calendar / Drive setup. Quit and reopen SHOGUN after saving.")
guard let idInput = getpass("Google Desktop OAuth Client ID (hidden): ") else { fail("Cannot read Client ID.") }
let clientId = String(cString: idInput).trimmingCharacters(in: .whitespacesAndNewlines)
guard clientId.range(of: "^[A-Za-z0-9_-]{1,200}\\.apps\\.googleusercontent\\.com$", options: .regularExpression) != nil else {
    fail("Paste the Google Desktop OAuth Client ID ending in .apps.googleusercontent.com.")
}
guard let secretInput = getpass("Google Desktop OAuth Client secret (hidden): ") else { fail("Cannot read Client secret.") }
let clientSecret = String(cString: secretInput).trimmingCharacters(in: .whitespacesAndNewlines)
guard !clientSecret.isEmpty, clientSecret.utf8.count <= 1024,
      !clientSecret.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else {
    fail("Paste the Client secret from the same Desktop OAuth client.")
}
requireFreshSetup()
// Add-only: competing setup or existing credentials cannot be overwritten.
addSecret(account: "google-oauth-client-id", value: clientId)
addSecret(account: "google-oauth-client-secret", value: clientSecret)
print("Google OAuth client saved in macOS Keychain. Reopen SHOGUN and Connect Gmail, Calendar or Drive. Google sign-in and a real provider read are still required.")
