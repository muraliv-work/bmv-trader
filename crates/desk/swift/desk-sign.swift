// desk-sign: the owner's Secure Enclave key for the Quant Desk (DECISION S30, DESIGN §4c).
// The private key is made inside the Secure Enclave and never leaves it; using it needs Touch ID or
// the Mac password. The desk keeps only the public key, so only the owner can approve.
//
//   desk-sign keygen <key-file>   make the key; prints the public key (hex)
//   desk-sign sign <key-file>     signs the text on stdin after Touch ID; prints the signature (hex)
//
// Build: scripts/build-desk-sign.sh
import CryptoKit
import Foundation
import LocalAuthentication

func fail(_ message: String) -> Never {
    FileHandle.standardError.write((message + "\n").data(using: .utf8)!)
    exit(1)
}

func hex(_ d: Data) -> String { d.map { String(format: "%02x", $0) }.joined() }

let args = CommandLine.arguments
guard args.count == 3, ["keygen", "sign"].contains(args[1]) else {
    fail("usage: desk-sign keygen|sign <key-file>")
}
let path = URL(fileURLWithPath: args[2])
guard SecureEnclave.isAvailable else { fail("this Mac has no Secure Enclave") }

if args[1] == "keygen" {
    if FileManager.default.fileExists(atPath: path.path) { fail("\(path.path) exists; refusing to replace the owner key") }
    var err: Unmanaged<CFError>?
    // Touch ID, or the Mac password when Touch ID can't be used
    guard let ac = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
                                                   [.privateKeyUsage, .userPresence], &err) else {
        fail("access control: \(String(describing: err))")
    }
    do {
        let key = try SecureEnclave.P256.Signing.PrivateKey(accessControl: ac)
        try FileManager.default.createDirectory(at: path.deletingLastPathComponent(), withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        // the file is an encrypted handle that only this Mac's Secure Enclave can use
        guard FileManager.default.createFile(atPath: path.path, contents: key.dataRepresentation,
                                             attributes: [.posixPermissions: 0o600]) else { fail("can't write \(path.path)") }
        print(hex(key.publicKey.rawRepresentation))
    } catch { fail("key: \(error)") }
} else {
    let text = String(decoding: FileHandle.standardInput.readDataToEndOfFile(), as: UTF8.self)
    guard !text.isEmpty, let blob = FileManager.default.contents(atPath: path.path) else { fail("no text, or no key at \(path.path)") }
    let context = LAContext()
    // the dialog names what is being approved: the request's action line
    let action = text.split(separator: "\n").first { $0.hasPrefix("action: ") }.map { String($0.dropFirst(8)) } ?? "register the owner key"
    context.localizedReason = "approve: \(action)"
    do {
        let key = try SecureEnclave.P256.Signing.PrivateKey(dataRepresentation: blob, authenticationContext: context)
        let sig = try key.signature(for: Data(text.utf8))
        print(hex(sig.rawRepresentation))
    } catch { fail("not signed: \(error)") }
}
