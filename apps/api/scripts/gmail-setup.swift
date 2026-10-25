// Native Keychain setup. Secrets arrive through a hidden terminal prompt, never argv/files.
import Foundation
import Security
import Darwin

func fail(_ message: String) -> Never {
    fputs(message + "\n", stderr)
    exit(1)
}
func saveSecret(service: String, account: String, bytes: Data) {
    let query: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
                              kSecAttrService as String: service, kSecAttrAccount as String: account]
    let update = SecItemUpdate(query as CFDictionary, [kSecValueData as String: bytes] as CFDictionary)
    if update == errSecItemNotFound {
        var item = query
        item[kSecValueData as String] = bytes
        item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        if SecItemAdd(item as CFDictionary, nil) != errSecSuccess { fail("Keychain save failed. Unlock login Keychain and retry.") }
    } else if update != errSecSuccess { fail("Keychain update failed. Unlock login Keychain and retry.") }
}
let directory = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/SHOGUN Gmail Backend")
let file = directory.appendingPathComponent("connection.json")
if isatty(STDIN_FILENO) != 1 { fail("Run gmail:setup in your terminal; interactive input required.") }
print("Stop the Gmail backend before setup. Secrets will be stored in macOS Keychain.")
var state: [String: Any] = ["userId": "shogun-local-" + UUID().uuidString.lowercased(),
                          "connectedAccountId": NSNull(), "consentGrantedAt": NSNull(), "lastVerifiedAt": NSNull(), "toolVersion": NSNull()]
if FileManager.default.fileExists(atPath: file.path) {
    guard let directoryInfo = try? directory.resourceValues(forKeys: [.isSymbolicLinkKey, .isDirectoryKey]),
          directoryInfo.isSymbolicLink == false, directoryInfo.isDirectory == true,
          let fileInfo = try? file.resourceValues(forKeys: [.isSymbolicLinkKey, .isRegularFileKey]),
          fileInfo.isSymbolicLink == false, fileInfo.isRegularFile == true else { fail("Unsafe metadata path. Refusing to follow a symlink.") }
    guard let existing = try? Data(contentsOf: file),
          let value = try? JSONSerialization.jsonObject(with: existing) as? [String: Any] else { fail("Connection metadata is unreadable. Do not overwrite it; inspect the runbook.") }
    state = value
    let allowed = Set(["userId", "authConfigId", "connectedAccountId", "consentGrantedAt", "lastVerifiedAt", "toolVersion", "lastReadLogId"])
    guard Set(state.keys).isSubset(of: allowed), let user = state["userId"] as? String,
          user.range(of: "^[A-Za-z0-9_-]{1,160}$", options: .regularExpression) != nil else { fail("Invalid connection metadata. Refusing to overwrite it.") }
}
print("Paste Gmail Auth Config ID (ac_...) from your Composio project:", terminator: " ")
guard let input = readLine()?.trimmingCharacters(in: .whitespacesAndNewlines),
      input.range(of: "^ac_[A-Za-z0-9_-]{1,150}$", options: .regularExpression) != nil else { fail("Invalid Gmail Auth Config ID.") }
if let old = state["authConfigId"] as? String, old != input, !(state["connectedAccountId"] is NSNull) {
    fail("Disconnect Gmail in SHOGUN before changing its Auth Config ID.")
}
state["authConfigId"] = input
state["toolVersion"] = NSNull()
guard let secret = getpass("Paste Composio project API key (hidden): ") else { fail("Could not read the key.") }
let key = String(cString: secret).trimmingCharacters(in: .whitespacesAndNewlines)
guard key.utf8.count >= 16, key.utf8.count <= 1024, !key.contains("\n") else { fail("API key looks incomplete.") }
saveSecret(service: "com.selectkk.shogun.gmail-backend", account: "composio-api-key", bytes: Data(key.utf8))
let bearerQuery: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
    kSecAttrService as String: "com.selectkk.shogun", kSecAttrAccount as String: "gmail-local-backend-token",
    kSecReturnData as String: true, kSecMatchLimit as String: kSecMatchLimitOne]
var bearerValue: CFTypeRef?
let bearerStatus = SecItemCopyMatching(bearerQuery as CFDictionary, &bearerValue)
if bearerStatus == errSecItemNotFound {
    var random = [UInt8](repeating: 0, count: 32)
    if SecRandomCopyBytes(kSecRandomDefault, random.count, &random) != errSecSuccess { fail("Secure random generation failed.") }
    let token = random.map { String(format: "%02x", $0) }.joined()
    saveSecret(service: "com.selectkk.shogun", account: "gmail-local-backend-token", bytes: Data(token.utf8))
} else if bearerStatus != errSecSuccess {
    fail("Local backend credential is inaccessible. Unlock Keychain and retry.")
} else {
    guard let bytes = bearerValue as? Data, let token = String(data: bytes, encoding: .utf8),
          token.range(of: "^[a-f0-9]{64}$", options: .regularExpression) != nil else {
        fail("Local credential is malformed. Remove only gmail-local-backend-token in Keychain Access, then rerun setup.")
    }
}
do {
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
    try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
    let data = try JSONSerialization.data(withJSONObject: state, options: [.sortedKeys])
    try data.write(to: file, options: [.atomic])
    try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
} catch { fail("Could not save non-secret connection metadata.") }
print("Setup saved. Project key and local bearer are in Keychain. No Gmail password or Google client secret needed.")
