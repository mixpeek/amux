import SwiftUI

struct ServerPickerView: View {
    @EnvironmentObject var serverManager: ServerManager
    @State private var customURL = ""
    @State private var customName = ""
    @State private var urlError = false

    var body: some View {
        NavigationStack {
            VStack(spacing: 0) {
                // Header
                VStack(spacing: 12) {
                    Image(systemName: "square.stack.3d.up.fill")
                        .font(.system(size: 56))
                        .foregroundColor(.accentColor)
                        .padding(.top, 48)
                    Text("amux")
                        .font(.largeTitle.bold())
                    Text("Connect to your amux server")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                .padding(.bottom, 36)

                // Server URL form
                VStack(spacing: 16) {
                    VStack(alignment: .leading, spacing: 8) {
                        TextField("Name (optional)", text: $customName)
                            .padding(12)
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .clipShape(RoundedRectangle(cornerRadius: 10))

                        TextField("https://amux.tail-xxxx.ts.net:8822", text: $customURL)
                            .keyboardType(.URL)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .padding(12)
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .clipShape(RoundedRectangle(cornerRadius: 10))

                        Text("Find your Tailscale hostname in the Tailscale app. Port is 8822 by default.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 4)
                    }

                    if urlError {
                        Text("Please enter a valid URL starting with http:// or https://")
                            .foregroundColor(.red)
                            .font(.caption)
                    }

                    Button(action: {
                        urlError = false
                        let name = customName.isEmpty ? customURL : customName
                        if serverManager.addServer(name: name, urlString: customURL) {
                            serverManager.selectServer(customURL)
                        } else {
                            urlError = true
                        }
                    }) {
                        Text("Connect")
                            .font(.headline)
                            .frame(maxWidth: .infinity)
                            .padding(14)
                            .background(Color.accentColor)
                            .foregroundColor(.white)
                            .clipShape(RoundedRectangle(cornerRadius: 12))
                    }
                    .disabled(customURL.isEmpty)
                }
                .padding(.horizontal, 20)

                Spacer()
            }
        }
    }
}
