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

                // amux Cloud: sign in and open your servers. The app sells nothing
                // (App Store guideline 3.1.3(f)): no prices, no links to buy.
                CloudSignInSection()

                // Divider
                HStack {
                    Rectangle().frame(height: 1).foregroundStyle(.quaternary)
                    Text("or self-hosted")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                        .layoutPriority(1)
                    Rectangle().frame(height: 1).foregroundStyle(.quaternary)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)

                // Server URL form
                VStack(spacing: 16) {
                    VStack(alignment: .leading, spacing: 8) {
                        TextField("Name (optional)", text: $customName)
                            .padding(12)
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .clipShape(RoundedRectangle(cornerRadius: 10))

                        TextField("https://amux.tail-xxxx.ts.net:8824", text: $customURL)
                            .keyboardType(.URL)
                            .autocorrectionDisabled()
                            .textInputAutocapitalization(.never)
                            .padding(12)
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .clipShape(RoundedRectangle(cornerRadius: 10))

                        Text("Find your Tailscale hostname in the Tailscale app. Port is 8824 by default.")
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
                            .background(Color(uiColor: .secondarySystemGroupedBackground))
                            .foregroundColor(.primary)
                            .clipShape(RoundedRectangle(cornerRadius: 12))
                    }
                    .disabled(customURL.isEmpty)
                }
                .padding(.horizontal, 20)

                Spacer()
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(uiColor: .systemBackground))
        }
    }
}
