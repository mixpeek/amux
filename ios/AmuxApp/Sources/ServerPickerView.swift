import SwiftUI

/// Where the cloud button sends people. One constant, because the app, the
/// site and the Cloudflare redirect all have to agree on it.
enum AmuxCloud {
    static let onboardingURL = URL(string: "https://amux.io/cloud/")!
}

struct ServerPickerView: View {
    @Environment(\.openURL) private var openURL
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

                // Cloud option.
                //
                // This used to add https://cloud.amux.io as a server and select
                // it, which sent a first-run user straight into the black
                // screen: that origin has been unreachable for days and the
                // picker has no way to know. Onboarding for cloud now goes
                // through the web page, where a human schedules it.
                //
                // OPENS EXTERNALLY rather than loading in the app's WebView,
                // because the destination is a marketing page and Calendly, not
                // an amux server. Loading it in the dashboard WebView would
                // leave the app pointed at something it cannot talk to.
                VStack(spacing: 12) {
                    Button(action: { openURL(AmuxCloud.onboardingURL) }) {
                        HStack {
                            Image(systemName: "cloud.fill")
                            Text("Get amux cloud")
                                .font(.headline)
                            Image(systemName: "arrow.up.right")
                                .font(.footnote.weight(.semibold))
                        }
                        .frame(maxWidth: .infinity)
                        .padding(14)
                        .background(Color.accentColor)
                        .foregroundColor(.white)
                        .clipShape(RoundedRectangle(cornerRadius: 12))
                    }

                    // The old caption said "Includes Sign in with Apple", which
                    // described the sign-in this button no longer performs.
                    Text("Opens amux.io to schedule onboarding")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 24)

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
